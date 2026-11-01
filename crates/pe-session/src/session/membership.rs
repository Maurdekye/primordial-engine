//! Membership: epochs, quorum rules, equivocation proofs and the view change (P2-4, design 10.8).

use super::*;

/// `valid_round` of a proposal that no quorum has prevoted before.
pub const NO_ROUND: u32 = u32::MAX;

/// Top bit of an encoded change's epoch: the change carries D80 admin entries and kicks.
pub(super) const CHANGE_EXT: u32 = 1 << 31;

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
    /// VC-5: an ack's epoch rises by at most one; an epoch newer than mine left unbacked is voted out.
    pub const VC5: u64 = 1 << 3;
    /// Round 3, VCR-1: a reset answers a kick vote that names no held, pending kick of its target.
    pub const VCR1: u64 = 1 << 4;
    /// VCR-2: a vote on its own kick freezes exactly its author's previous entry; a cut uses only
    /// votes whose frozen heads this replica can walk.
    pub const VCR2: u64 = 1 << 5;
    /// VCR-3: a member acks as soon as it enters a new epoch, so its acks never skip one.
    pub const VCR3: u64 = 1 << 6;
    /// VCR-4: newer-epoch timers survive decisions. (Round 3 also counted resets against the members
    /// whose votes and caps they overruled; that part is now `VC3R2`'s.)
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
    /// Round 4, VC3R-1: the newer-epoch timer runs whatever other members' acks show. Round 3 paused
    /// it while this replica looked behind, which a climbing member could keep true for good.
    pub const VC3R1: u64 = 1 << 15;
    /// VC3R-2: no strikes. A stale cap, or a kick vote whose kick nobody delivers, gets its author
    /// voted out instead of answered by a reset, and a reset waits briefly for votes to gather.
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
    /// author for a silence limit at most, after which an undelivered claim gets its maker voted out.
    pub const VC4R3: u64 = 1 << 23;
    /// VC4R-1: the cap rule counts a prevote in any version this replica holds.
    pub const VC4R1: u64 = 1 << 24;
    /// VC4R-4: entry fetches by id are retried with backoff instead of asked once.
    pub const VC4R4: u64 = 1 << 25;
    /// VC4R-5: a vote whose frozen head nobody delivers stops holding resets back, and its voter is
    /// voted out.
    pub const VC4R5: u64 = 1 << 26;
    /// VC4R-6: a join request's signature names the session and an expiry.
    pub const VC4R6: u64 = 1 << 27;
    /// Round 5, VC3R-3's low: a listed admin entry must be stable here (every member holds that version).
    pub const VC3R3S: u64 = 1 << 28;
    /// Round 7, VC6R-1: a capper whose polka names prevotes nobody delivers within a further half window
    /// is voted out, instead of the fetch being retried for good.
    pub const VC6R1: u64 = 1 << 29;
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
    /// VC7R-2: evidence an honest member must deliver gets the slow-member allowance (`suspect_after`)
    /// before its author is voted out: a cap's polka (VC6R-1) and a vote's frozen head (VC4R-5). The
    /// cap's polka also counts every prevote the precommit named that is held, linked or not.
    pub const VC7R2: u64 = 1 << 34;
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
    /// Option 2's hole rule (the R18 below-20 item): a signed run still waiting on its author's entry
    /// below after the hold asks everyone for the whole range, so a fork's other version arrives even
    /// when the sender asked first is the forker. Off: only the sender is asked for the whole range.
    pub const HOLE: u64 = 1 << 41;
}

/// The most attestations a join request or a change carries per joiner (review VCR-11).
pub const MAX_ATTESTS: usize = 64;

/// Review VC6R-2: the most prevote ids a capped precommit carries (its polka). At least the largest
/// quorum, so honest code never exceeds it; a decode of more is refused.
pub const MAX_POLKA: usize = 512;

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
    pub add_reqs: Vec<(Sig, u64)>,
}

/// Commit order of admin entries (D80): stamp, then author, then seq.
pub(super) fn admin_key(e: &Entry) -> (u64, MemberId, u32) {
    (e.stamp, e.member, e.seq)
}

/// The membership state an admin sequence runs against (D80).
#[derive(Clone)]
pub(super) struct AdminRun {
    pub(super) members: BTreeSet<MemberId>,
    pub(super) roles: BTreeMap<MemberId, RoleId>,
    pub(super) defs: BTreeMap<RoleId, u64>,
    excluded: BTreeSet<MemberId>,
    /// The world owners' keys (12.5).
    pub(super) owners: BTreeSet<Key>,
    /// The members' ownership keys ([`WorldSim::key_of`]), for Q27.
    pub(super) keys: BTreeMap<MemberId, Key>,
    /// 12.4: this session's bans and rejoin roles, by identity key.
    pub(super) bans: BTreeMap<Key, Restrict>,
    /// 12.4: the world's perma-bans (its ACL), by identity key.
    pub(super) perma: BTreeSet<Key>,
    /// The world's ownership chain (12.5), if it has one, and the session lineage that commits to it.
    book: Option<Book>,
    lineage: u32,
}

impl AdminRun {
    pub(super) fn perms(&self, m: MemberId) -> u64 {
        match self.roles.get(&m) {
            Some(&ADMIN) => perm::ALL,
            Some(r) => self.defs.get(r).copied().unwrap_or(0),
            None => 0,
        }
    }
    /// Whether `m` counts as an admin: its role holds the roles permission, so it can make itself Admin
    /// (V2 = b, user 08:31Z). Admin holds every permission, so this includes it. The last-admin rules
    /// count these members (@coordinator-opus's call, 09:24Z).
    pub(super) fn admin_like(&self, m: MemberId) -> bool {
        self.perms(m) & perm::ROLES != 0
    }
    pub(super) fn role_admin_like(&self, r: RoleId) -> bool {
        r == ADMIN || self.defs.get(&r).is_some_and(|p| p & perm::ROLES != 0)
    }
    pub(super) fn has_admin(&self) -> bool {
        self.members.iter().any(|&m| self.admin_like(m))
    }
    /// Whether member `a` may kick `target` in this state: the kick permission, not itself, a member;
    /// only a co-owner kicks an owner (Q27, user 22:49Z); kicking a member who counts as an admin needs
    /// its own permission (V1, user 08:31Z). `apply` judges a kick by it in commit order, and a replica
    /// receiving a kick by it against the current state, so a kick refused now freezes no one.
    pub(super) fn kick_allowed(&self, a: MemberId, target: MemberId) -> bool {
        let p = self.perms(a);
        let owner_safe = self.owns(target) && !self.owns(a);
        let admin_safe = self.admin_like(target) && p & perm::KICK_ADMIN == 0;
        p & perm::KICK != 0 && target != a && self.members.contains(&target) && !owner_safe && !admin_safe
    }
    /// Member `m` owns the world, as this point of the sequence has it (12.5, Q27).
    fn owns(&self, m: MemberId) -> bool {
        self.keys.get(&m).is_some_and(|k| self.owners.contains(k))
    }
    /// Whether member `a` may kick `target` with option `rejoin` (12.4 kick options): the kick itself
    /// ([`Self::kick_allowed`]), and for an option the target's key; a rejoin role must be defined and
    /// not count as an admin; a ban needs the ban's own right ([`Self::may_ban`]), room in its list, and
    /// a target that is no owner (row 0 is above the bans). Otherwise the whole kick is refused.
    pub(super) fn kick_ok(&self, a: MemberId, target: MemberId, rejoin: Rejoin) -> bool {
        if !self.kick_allowed(a, target) {
            return false;
        }
        let Some(k) = self.keys.get(&target) else { return rejoin == Rejoin::Free };
        match rejoin {
            Rejoin::Free => true,
            Rejoin::Role(r) => r != ADMIN && self.defs.contains_key(&r) && !self.role_admin_like(r),
            Rejoin::Ban | Rejoin::PermaBan => {
                let perma = rejoin == Rejoin::PermaBan;
                self.may_ban(a, perma) && !self.owners.contains(k) && self.ban_room(k, perma)
            }
        }
    }
    /// Who may ban or lift a ban (12.4, user msg m5c4b84d186a8): this session's, the BAN permission
    /// (Admin holds it); the world's perma-bans, an owner, whatever its role.
    fn may_ban(&self, a: MemberId, perma: bool) -> bool {
        if perma { self.owns(a) } else { self.perms(a) & perm::BAN != 0 }
    }
    fn banned(&self, k: &Key, perma: bool) -> bool {
        if perma { self.perma.contains(k) } else { self.bans.get(k) == Some(&Restrict::Ban) }
    }
    /// Whether a member holds identity key `k` at this point of the sequence.
    pub(super) fn holds(&self, k: &Key) -> bool {
        self.members.iter().any(|m| self.keys.get(m) == Some(k))
    }
    /// Whether banning `k` leaves its list within [`MAX_BANS`].
    fn ban_room(&self, k: &Key, perma: bool) -> bool {
        let n = if perma { self.perma.len() } else { self.bans.values().filter(|r| **r == Restrict::Ban).count() };
        self.banned(k, perma) || n < MAX_BANS
    }
    /// Whether member `a` may make ban entry (`key`, `on`, `perma`) (12.4): the ban's right; a ban names
    /// a key that no member and no owner holds and its list doesn't hold yet, within [`MAX_BANS`]; a lift
    /// names a key its list holds (a session lift: a ban or a rejoin role).
    pub(super) fn ban_ok(&self, a: MemberId, key: &Key, on: bool, perma: bool) -> bool {
        if !self.may_ban(a, perma) {
            return false;
        }
        match (on, perma) {
            (true, _) => !self.holds(key) && !self.owners.contains(key) && !self.banned(key, perma) && self.ban_room(key, perma),
            (false, true) => self.perma.contains(key),
            (false, false) => self.bans.contains_key(key),
        }
    }
    /// Whether admin entry `e` applies at this point of the sequence; if it does, apply it. A
    /// refused entry changes nothing.
    pub(super) fn apply(&mut self, e: &Entry) -> bool {
        let a = e.member;
        if !self.members.contains(&a) || self.excluded.contains(&a) {
            return false;
        }
        let p = self.perms(a);
        match e.payload {
            Payload::Kick { target, rejoin, .. } => {
                if !self.kick_ok(a, target, rejoin) {
                    return false;
                }
                if let Some(&k) = self.keys.get(&target) {
                    restrict(&mut self.bans, &mut self.perma, k, rejoin);
                }
                self.members.remove(&target);
                self.roles.remove(&target);
                true
            }
            Payload::Ban { key, on, perma, .. } => {
                if !self.ban_ok(a, &key, on, perma) {
                    return false;
                }
                ban(&mut self.bans, &mut self.perma, key, on, perma);
                true
            }
            Payload::SetRole { member, role, .. } => {
                // Q27 (user 22:49Z): an owner may give up Admin, but only a co-owner demotes an owner.
                let owner_safe = self.owns(member) && member != a && !self.owns(a) && role != ADMIN;
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
                // 12.4: an admin's explicit promotion lifts the member's rejoin role.
                if self.role_admin_like(role)
                    && let Some(k) = self.keys.get(&member)
                {
                    lift_rejoin_role(&mut self.bans, k);
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

/// What a session's ban list holds for one identity key (12.4): a ban, or a rejoin role (kick and
/// remove Admin). Session state only: never written into the world (user msg m5c4b84d186a8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Restrict {
    Ban,
    Role(RoleId),
}

/// Each ban list (a session's, the world's perma-bans) holds at most this many keys (12.4).
pub const MAX_BANS: usize = 4096;

/// Record a kick's option for key `k`: in the session's list a ban replaces anything and a rejoin role
/// replaces nothing but a rejoin role; a perma-ban goes into the world's list, beside the session's.
fn restrict(bans: &mut BTreeMap<Key, Restrict>, perma: &mut BTreeSet<Key>, k: Key, rejoin: Rejoin) {
    match rejoin {
        Rejoin::Free => {}
        Rejoin::Ban => {
            bans.insert(k, Restrict::Ban);
        }
        Rejoin::PermaBan => {
            perma.insert(k);
        }
        Rejoin::Role(r) => {
            if bans.get(&k) != Some(&Restrict::Ban) {
                bans.insert(k, Restrict::Role(r));
            }
        }
    }
}

/// Apply a ban entry: ban or lift `k` in the session's list or the world's. A session lift clears a
/// rejoin role too; the two lists never touch each other.
fn ban(bans: &mut BTreeMap<Key, Restrict>, perma: &mut BTreeSet<Key>, k: Key, on: bool, world: bool) {
    match (on, world) {
        (true, false) => {
            bans.insert(k, Restrict::Ban);
        }
        (false, false) => {
            bans.remove(&k);
        }
        (true, true) => {
            perma.insert(k);
        }
        (false, true) => {
            perma.remove(&k);
        }
    }
}

/// Lift `k`'s rejoin role (not a ban): a promotion to a role that counts as an admin.
fn lift_rejoin_role(acl: &mut BTreeMap<Key, Restrict>, k: &Key) {
    if matches!(acl.get(k), Some(Restrict::Role(_))) {
        acl.remove(k);
    }
}

/// A role in a session (D80; user rulings 22:13Z-22:18Z). Admin, role 0, is built in: it always
/// exists, holds every permission and is by default the only role. Admins define other roles as
/// subsets of the permission set. Admins are equal: none is above another.
pub type RoleId = u16;
pub const ADMIN: RoleId = 0;

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
    /// Ban and unban identity keys for this session (12.4; any Admin, user msg m5c4b84d186a8). A kick
    /// with the ban option needs it too. Perma-bans need an owner's key instead.
    pub const BAN: u64 = 1 << 3;
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
    pub(super) fn tag(self) -> u8 {
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
    pub sig_with: Sig,
    pub sig_joiner: Sig,
}

pub fn fact_rel(rel: Rel, joiner: MemberId, with: MemberId, world: u32) -> Fact {
    let (a, b) = if rel == Rel::Friend { (joiner.min(with), joiner.max(with)) } else { (with, joiner) };
    let w = if rel == Rel::Invited { world } else { 0 };
    FactWriter::new(FactKind::Rel).u32(rel.tag() as u32).u32(a).u32(b).u32(w).finish()
}

/// The attestation `with` and the joiner sign for `rel`, made with their test identities: a scenario
/// fixture. In a session each party signs its half with its own key (S3c).
pub fn attest(rel: Rel, joiner: MemberId, with: MemberId, world: u32) -> Attest {
    let f = fact_rel(rel, joiner, with, world);
    Attest { rel, with, sig_with: test_fact_sig(with, 0, f), sig_joiner: test_fact_sig(joiner, 0, f) }
}

pub fn attest_ok(keys: &PublicKeys, a: &Attest, joiner: MemberId, world: u32) -> bool {
    let f = fact_rel(a.rel, joiner, a.with, world);
    matches!(a.rel, Rel::Friend | Rel::Confidant | Rel::Invited)
        && a.with != joiner
        && keys.fact_ok(a.with, 0, f, &a.sig_with)
        && keys.fact_ok(joiner, 0, f, &a.sig_joiner)
}

/// Round 4's join fact (review VC3R-4): "`joiner` asks to join world `world`". Replayable in any later
/// session of the world (VC4R-6); kept for the fix-off runs.
pub fn fact_join(world: u32, joiner: MemberId) -> Fact {
    FactWriter::new(FactKind::Join).u32(world).u32(joiner).finish()
}

/// Round 4's join signature (`joiner`'s test identity over [`fact_join`]): what a fix-off scenario
/// replays.
pub fn join_sig(world: u32, joiner: MemberId) -> Sig {
    test_fact_sig(joiner, world, fact_join(world, joiner))
}

/// `joiner`'s test-identity signature on a request to join session `session` until `expires`
/// (review VC4R-6; production signs with [`KeyRing::sign_join`]).
pub fn join_sig_in(session: &SessionId, joiner: MemberId, expires: u64) -> Sig {
    test_join_sig(joiner, session, expires)
}

/// A request from `joiner` to join session `session`, signed with its test identity.
pub fn join_msg(session: &SessionId, joiner: MemberId, attests: Vec<Attest>, expires: u64) -> Msg {
    Msg::Join { member: joiner, attests, sig: join_sig_in(session, joiner, expires), expires }
}

/// The same request signed with test identity `identity`'s key ([`KeyRing::test_with`]).
pub fn join_msg_as(session: &SessionId, joiner: MemberId, identity: u32, attests: Vec<Attest>, expires: u64) -> Msg {
    use ed25519_dalek::Signer;
    let sig = crate::sign::test_signing_key(identity).sign(&crate::sign::join_message(session, joiner, expires)).to_bytes();
    Msg::Join { member: joiner, attests, sig, expires }
}

impl Change {
    /// The value the view change votes on: a hash of the whole change, never 0 (0 is nil). BLAKE3
    /// (review P23-2), 64 bits wide: a proposer that grinds two changes to one value (a birthday
    /// search, about 2^32) is an open item (the value widens to 256 bits with the vote payloads).
    pub fn value(&self) -> u64 {
        let mut w = Wr(Vec::with_capacity(64));
        w.change(self);
        hash64("primordial-engine 2026 vc change value v1", &w.0) | 1
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

/// The fact a member signs with its precommit of `value` in `round` of the view change that ends
/// epoch `epoch` (S3c; signed in the world's fact domain): an epoch certificate carries these.
pub fn vc_fact(epoch: u32, round: u32, value: u64) -> Fact {
    FactWriter::new(FactKind::VcValue).u32(epoch).u32(round).u64(value).finish()
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
    pub precommits: Vec<(MemberId, Sig)>,
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
            v.push(*m as u64);
            v.extend(sig_words(s));
        }
        v.extend(self.proofs.iter().flat_map(|e| e.id().words()));
        hash64("primordial-engine 2026 epoch cert hash v1", &v.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>())
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
pub fn epoch_cert_ok(keys: &PublicKeys, rule: QuorumRule, prev: &[MemberId], cert: &EpochCert) -> bool {
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
        .filter(|(m, s)| base.contains(m) && keys.fact_ok(*m, cert.world, vc_fact(c.epoch, cert.round, value), s))
        .map(|(m, _)| *m)
        .collect();
    let mut expect: BTreeSet<MemberId> = prev.iter().copied().collect();
    for r in &c.remove {
        expect.remove(&r.member);
    }
    expect.extend(c.add.iter().copied());
    signers.len() >= q && expect.into_iter().collect::<Vec<_>>() == cert.members
}

impl WorldSim {
    // ---- view change (P2-4, design 10.8) ---------------------------------------------------------

    /// Record a validated membership entry of the current epoch, in chain order.
    pub(super) fn record_vc(&mut self, m: MemberId, id: EntryId, out: &mut Vec<Entry>) {
        if self.is_static() {
            return;
        }
        if !self.vcm.members.contains(&m) {
            return;
        }
        let e = self.entries[&id].clone();
        match e.payload {
            Payload::Vote { target, frozen, kick, .. } => {
                if (target == m && kick == EntryId::NONE) || !self.vcm.members.contains(&target) || self.vcm.votes.get(&target).is_some_and(|v| v.contains_key(&m)) {
                    return;
                }
                // Review VCR-2: a member's vote on its own kick freezes exactly the entry before it. No
                // reference of its own can check that head, so any other head is refused.
                if target == m && self.fixes_off & fix::VCR2 == 0 && frozen != (e.seq - 1, e.prev) {
                    return;
                }
                let rec = VoteRec { frozen, stamp: e.stamp, id, kick, at: self.now };
                self.vcm.votes.entry(target).or_default().insert(m, rec);
                // Review VC3R-2: a kick vote names its kick; fetch one not held, from its voter and
                // everyone, so a real kick arrives long before its voter could count as a ghost.
                if kick != EntryId::NONE && !self.entries.contains_key(&kick) && self.fixes_off & fix::VC3R2 == 0 {
                    self.want_entry_all(kick, 0);
                }
                if let VoteCheck::Lie(ack) = self.vote_check(target, m, frozen) {
                    self.on_lie(m, ack, id);
                    if self.vcm.members.contains(&self.me) && !self.voted(m) && m != self.me {
                        out.extend(self.freeze_and_vote(m, e.seq, EntryId::NONE));
                    }
                }
            }
            Payload::Propose { round, valid_round, ref change, .. } => {
                self.note_proposal(id);
                if m == self.proposer(self.vcm.epoch, round) {
                    self.vcm.vc.proposals.entry(round).or_insert((change.value(), valid_round));
                    self.vcm.vc.seen_at.entry(change.value()).or_insert(self.now);
                }
            }
            Payload::Prevote { round, value, .. } => {
                self.vcm.vc.prevotes.entry(round).or_default().entry(m).or_insert(value);
                self.vcm.vc.any_prevotes.entry((round, value)).or_default().insert(m);
                self.vcm.vc.prevote_id.entry((round, m)).or_insert(id);
            }
            Payload::Precommit { epoch, round, value, boundary, ref polka, sig } => {
                // S3c: a precommit counts only with a valid signature over its fact, which the
                // epoch certificate then carries.
                if !self.keys.public.fact_ok(m, self.world_id, vc_fact(epoch, round, value), &sig) {
                    self.stats.bad_sigs += 1;
                    return;
                }
                self.vcm.vc.precommit_sigs.entry((round, value, m)).or_insert(sig);
                self.vcm.vc.precommits.entry(round).or_default().entry(m).or_insert(value);
                if boundary > 0 {
                    // Keep the first precommit's seq and the smallest boundary (review VC-10).
                    let c = self.vcm.join_caps.entry(m).or_insert((e.seq, boundary));
                    c.1 = c.1.min(boundary);
                    self.cap_seen.entry(m).or_insert((round, value, self.now));
                    // Review VC4R-1: keep the polka ids it named, so the next cap check fetches and
                    // counts them (in any version) before voting it out.
                    self.cap_polka.entry(m).or_insert_with(|| polka.clone());
                }
            }
            Payload::Kick { target, rejoin, .. } => {
                // V1: judged by the same rule as in commit order, against the current state, so a kick
                // that would be refused (an admin by a kick-only role, an owner by a non-owner) freezes
                // no one. It can be sent again once the state allows it.
                if self.vcm.admin_outcomes.contains_key(&id) || !self.admin_run().kick_ok(m, target, rejoin) {
                    return;
                }
                self.vcm.admin_pending.insert(id);
                // The kick decides; every member, the target too, only freezes the target's chain
                // so the view change can cut it (D80).
                if self.vcm.members.contains(&self.me) && !self.voted(target) && self.vcm.closed_at.is_none() {
                    if target == self.me {
                        out.extend(self.self_kick_vote(id));
                    } else {
                        out.extend(self.freeze_and_vote(target, 0, id));
                    }
                }
            }
            Payload::SetRole { .. } | Payload::DefineRole { .. } => {
                if !self.vcm.admin_outcomes.contains_key(&id) && self.perms_of(m) & perm::ROLES != 0 {
                    self.vcm.admin_pending.insert(id);
                }
            }
            // 12.4: pending only if it would apply now (V1's receipt rule, V13-1).
            Payload::Ban { key, on, perma, .. } => {
                if !self.vcm.admin_outcomes.contains_key(&id) && self.admin_run().ban_ok(m, &key, on, perma) {
                    self.vcm.admin_pending.insert(id);
                }
            }
            // 12.5: pending only while the certificate would apply to the book as decided (V1's
            // receipt rule): an entry that cannot apply would start view changes for nothing (VC6R-4).
            // Re-sent later, it is a new entry.
            Payload::Ownership { ref cert, .. } if !self.vcm.admin_outcomes.contains_key(&id) && self.vcm.closed_at.is_none() && self.vcm.members.contains(&m) => {
                match self.ownership_judge(cert) {
                    Ok(Insert::Applied) => {
                        self.vcm.admin_pending.insert(id);
                    }
                    // Review OWNY-2: its parent may be a pending certificate; hold it until it is decided.
                    Err(OwnError::MissingParent) if self.fixes_off & fix::OWNY2 == 0 => {
                        let mine = self.vcm.own_held.iter().filter(|h| self.entries.get(h).is_some_and(|e| e.member == m)).count();
                        if mine < MAX_HELD_PER_AUTHOR {
                            self.vcm.own_held.insert(id);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// Review HO-1: `obj` belongs to a member other than `author` (LOGIC acts for the world). Hands
    /// today; avatar parts and any per-member object later.
    pub(super) fn not_owner(&self, obj: u32, author: MemberId) -> bool {
        author != LOGIC && self.hand_owners.get(&obj).is_some_and(|&o| o != author)
    }

    /// D80: the permissions member `m` holds in this epoch.
    pub fn perms_of(&self, m: MemberId) -> u64 {
        self.admin_run().perms(m)
    }

    /// Review VC4R-6: this session's id, which a join request must name.
    pub fn session(&self) -> SessionId {
        self.keys.session
    }

    /// Whether `sig` is joiner `j`'s signature on a request to join this session until `expires`
    /// (review VC4R-6). With VC4R-6 off: round 4's world-bound fact, replayable in a later session.
    pub(super) fn join_sig_ok(&self, j: MemberId, sig: &Sig, expires: u64) -> bool {
        if self.fixes_off & fix::VC4R6 != 0 {
            self.keys.public.fact_ok(j, self.world_id, fact_join(self.world_id, j), sig)
        } else {
            self.keys.public.join_ok(j, &self.keys.session, expires, sig)
        }
    }

    /// D80: this epoch's admins.
    pub fn admins(&self) -> Vec<MemberId> {
        self.vcm.members.iter().copied().filter(|m| self.vcm.roles.get(m) == Some(&ADMIN)).collect()
    }

    pub(super) fn admin_run(&self) -> AdminRun {
        AdminRun {
            members: self.vcm.members.clone(),
            roles: self.vcm.roles.clone(),
            defs: self.vcm.role_defs.clone(),
            excluded: BTreeSet::new(),
            owners: self.owner_keys(),
            keys: self.vcm.members.iter().filter_map(|&m| Some((m, self.key_of(m)?))).collect(),
            bans: self.vcm.bans.clone(),
            perma: self.vcm.perma_bans.clone(),
            book: self.vcm.book.clone(),
            lineage: self.world_id,
        }
    }

    /// Member `m`'s ownership key (12.5): its Ed25519 identity key from the session's key directory.
    /// Production: the key its join certificate carries, signed by it [I]. None without a key (an
    /// offline replay holds none), and a member without a key owns nothing.
    pub fn key_of(&self, m: MemberId) -> Option<Key> {
        self.keys.public.get(m).map(|k| Key(k.to_bytes()))
    }

    /// The world owners' keys (12.5): from the ownership chain if the world has one, else the keys of
    /// the owners it lists.
    pub fn owner_keys(&self) -> BTreeSet<Key> {
        match &self.vcm.book {
            Some(b) => b.view().owners.owners,
            None => self.vcm.owners.iter().filter_map(|&m| self.key_of(m)).collect(),
        }
    }

    /// How `cert` would fare against the book as decided now (12.5): it applies if its owners'
    /// signatures check and it builds on the current heads.
    pub(super) fn ownership_judge(&self, cert: &Cert) -> Result<Insert, OwnError> {
        match self.vcm.book.clone() {
            Some(mut b) => b.commit_local(cert.clone(), self.world_id, self.frame),
            None => Err(OwnError::NotAllowed),
        }
    }

    pub(super) fn ownership_applies(&self, cert: &Cert) -> bool {
        matches!(self.ownership_judge(cert), Ok(Insert::Applied))
    }

    /// 12.5: publish an ownership certificate (an admin entry); every replica judges it.
    pub fn publish_ownership(&mut self, cert: Cert) -> SignedEntry {
        let epoch = self.vcm.epoch;
        let e = self.author_vc(Payload::Ownership { epoch, cert });
        self.signed(e)
    }

    /// The decided view changes, in order (the ownership report replays their admin entries).
    pub fn decided_changes(&self) -> impl Iterator<Item = &Change> {
        self.vcm.decided.iter().map(|(c, _, _)| c)
    }

    /// Give the world an ownership chain from its genesis certificate (part of the genesis
    /// document; before the first frame is simulated).
    pub fn set_ownership(&mut self, genesis: Cert) {
        assert_eq!(self.frame, 0, "ownership is part of the world's genesis");
        self.vcm.book = Some(Book::new(genesis).expect("a valid genesis certificate"));
    }

    /// A certificate for `change` on top of this replica's present ownership head(s), signed by test
    /// identities `signers` (the harness: a key holder handing the member a signed certificate). A
    /// merge joins every head; any other change builds on the first head.
    pub fn owner_cert(&self, change: OwnChange, signers: &[u32]) -> Option<Cert> {
        let b = self.vcm.book.as_ref()?;
        let heads = b.heads();
        let prev = if change == OwnChange::Merge { heads } else { heads.into_iter().take(1).collect() };
        let seq = prev.iter().map(|p| b.cert(*p).map_or(0, |c| c.seq)).max()? + 1;
        Some(Cert { world: b.world, seq, prev, change, sigs: vec![] }.signed_test(signers))
    }

    /// D80: kick `target` (the harness or an admin's UI). Authored whatever this member's role;
    /// every replica judges it.
    pub fn kick(&mut self, target: MemberId) -> SignedEntry {
        self.kick_with(target, Rejoin::Free)
    }

    /// 12.4: kick `target` with an option: remove Admin (a rejoin role) or ban.
    pub fn kick_with(&mut self, target: MemberId, rejoin: Rejoin) -> SignedEntry {
        let epoch = self.vcm.epoch;
        let e = self.author_vc(Payload::Kick { epoch, target, rejoin });
        self.signed(e)
    }

    /// 12.4: ban identity key `key` (no member holds it) for this session or (`perma`) for the world,
    /// or lift that ban (a session lift also clears a rejoin role).
    pub fn ban(&mut self, key: Key, on: bool, perma: bool) -> SignedEntry {
        let epoch = self.vcm.epoch;
        let e = self.author_vc(Payload::Ban { epoch, key, on, perma });
        self.signed(e)
    }

    /// 12.4: the world's perma-bans, as decided: what an owner's client writes into `acl/` (16.10), and
    /// what the world's next session starts from ([`Self::set_perma_bans`]). This session's own bans
    /// and rejoin roles (`vcm.bans`) are never written.
    pub fn perma_bans(&self) -> &BTreeSet<Key> {
        &self.vcm.perma_bans
    }

    /// Start the session from the world's saved perma-bans (part of its starting governance, before
    /// the first frame is simulated). A session starts with no session bans.
    pub fn set_perma_bans(&mut self, keys: BTreeSet<Key>) {
        assert_eq!(self.frame, 0, "the perma-bans are part of the session's starting governance");
        self.vcm.perma_bans = keys;
    }

    /// D80: give `member` the role `role`.
    pub fn set_role(&mut self, member: MemberId, role: RoleId) -> SignedEntry {
        let epoch = self.vcm.epoch;
        let e = self.author_vc(Payload::SetRole { epoch, member, role });
        self.signed(e)
    }

    /// D80: define custom role `role`.
    pub fn define_role(&mut self, role: RoleId, perms: u64) -> SignedEntry {
        let epoch = self.vcm.epoch;
        let e = self.author_vc(Payload::DefineRole { epoch, role, perms });
        self.signed(e)
    }

    /// A member's freeze of its own chain when a valid kick targets it (D80): it lets a kick go
    /// through where the other members alone are no quorum, as in a session of two. Its head is the
    /// longest, so it never moves the cut past what the other voters froze.
    pub(super) fn self_kick_vote(&mut self, kick: EntryId) -> Option<Entry> {
        if self.vcm.withhold_vc {
            return None;
        }
        let v = Entry {
            member: self.me,
            seq: self.my_seq + 1,
            prev: self.my_last,
            stamp: self.frame.max(self.now),
            payload: Payload::Vote { epoch: self.vcm.epoch, target: self.me, seq: 0, frozen: (self.my_seq, self.my_last), kick },
            carried: None,
        };
        Some(self.publish(v))
    }

    /// Whether member `j` may join now under the join rules, against `run` (the membership the
    /// change leaves before its joins), and with which starting role (D80).
    pub(super) fn join_role(&self, j: MemberId, attests: &[Attest], run: &AdminRun) -> Option<RoleId> {
        // Row 0, fixed in every world (user 22:48Z): a world owner joins as Admin. Owners, like the
        // other targets, are as the admin entries before the join in its change leave them (review OWNY-1).
        let jk = self.key_of(j);
        // 12.4 (review BN-3): one key, one member. A key a member holds does not join again.
        if jk.is_some_and(|k| run.holds(&k)) {
            return None;
        }
        if jk.is_some_and(|k| run.owners.contains(&k)) {
            return Some(ADMIN);
        }
        // 12.4: the perma-bans, then the session's bans, after row 0 and before the rows; a rejoin role
        // replaces a role that counts as an admin (and refuses if it no longer qualifies).
        if jk.is_some_and(|k| run.perma.contains(&k)) {
            return None;
        }
        let limit = match jk.and_then(|k| run.bans.get(&k)) {
            Some(Restrict::Ban) => return None,
            Some(&Restrict::Role(r)) => Some(r),
            None => None,
        };
        let role = self.join_rows(j, jk, attests, run)?;
        match limit {
            Some(r) if run.role_admin_like(role) => (r != ADMIN && run.defs.contains_key(&r) && !run.role_admin_like(r)).then_some(r),
            _ => Some(role),
        }
    }

    /// The join rows after row 0 and the ban list, then the fallback (D80).
    fn join_rows(&self, j: MemberId, jk: Option<Key>, attests: &[Attest], run: &AdminRun) -> Option<RoleId> {
        let has = |rel: Rel, g: MemberId| match rel {
            Rel::Is => g == j,
            Rel::Named(n) => n == j,
            _ => attests.iter().any(|a| a.rel == rel && a.with == g && attest_ok(&self.keys.public, a, j, self.world_id)),
        };
        // An owner is a key (12.5), who need not be a member: a relationship counts with an owner if the
        // member it names holds that owner's key. `Named(n)` names the joiner, as for every target
        // (review OWNP-1).
        let has_owner = |rel: Rel, o: Key| match rel {
            Rel::Is => jk == Some(o),
            Rel::Named(n) => n == j,
            _ => attests.iter().any(|a| a.rel == rel && self.key_of(a.with) == Some(o) && attest_ok(&self.keys.public, a, j, self.world_id)),
        };
        for rule in &self.vcm.join_rules.rules {
            let hit = match rule.target {
                Target::Owner if rule.all => !run.owners.is_empty() && run.owners.iter().all(|&o| has_owner(rule.rel, o)),
                Target::Owner => run.owners.iter().any(|&o| has_owner(rule.rel, o)),
                _ => {
                    let group: Vec<MemberId> = match rule.target {
                        Target::Player => run.members.iter().copied().collect(),
                        Target::Admin => run.members.iter().copied().filter(|m| run.roles.get(m) == Some(&ADMIN)).collect(),
                        Target::Role(r) => run.members.iter().copied().filter(|m| run.roles.get(m) == Some(&r)).collect(),
                        Target::Owner => unreachable!(),
                    };
                    if rule.all { !group.is_empty() && group.iter().all(|&g| has(rule.rel, g)) } else { group.iter().any(|&g| has(rule.rel, g)) }
                }
            };
            if hit {
                return rule.role;
            }
        }
        self.vcm.join_rules.fallback
    }

    /// The usable votes against `x` among `base`, for a removal: any vote for a kick's removal, only
    /// votes that answer no kick for a removal on suspicion or proof (D80).
    pub(super) fn removal_votes(&mut self, x: MemberId, base: &BTreeSet<MemberId>, kick: bool) -> Vec<(MemberId, VoteRec)> {
        let recs: Vec<(MemberId, VoteRec)> = self
            .vcm.votes
            .get(&x)
            .map(|v| v.iter().filter(|(v, r)| base.contains(v) && (kick || r.kick == EntryId::NONE)).map(|(v, r)| (*v, *r)).collect())
            .unwrap_or_default();
        recs.into_iter().filter(|(v, r)| self.vote_check(x, *v, r.frozen) == VoteCheck::Valid).collect()
    }

    /// Review VCR-1: whether `kick` is a held, pending kick of `target`, so a vote naming it freezes
    /// the target's chain for that kick's cut.
    pub(super) fn kick_live(&self, target: MemberId, kick: EntryId) -> bool {
        self.vcm.admin_pending.contains(&kick) && matches!(self.entries.get(&kick).map(|e| &e.payload), Some(Payload::Kick { target: t, .. }) if *t == target)
    }

    /// Review VCR-2: whether this replica can walk `x`'s chain down from `head` to the prefix it holds,
    /// as a cut computed from that head needs.
    pub(super) fn head_held(&self, x: MemberId, head: (u32, EntryId)) -> bool {
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
    pub(super) fn held(&self, m: MemberId) -> bool {
        self.fixes_off & (fix::VC3R2 | fix::LAGKEEP) == 0 && self.vote_hold.get(&m).is_some_and(|&t| self.now < t)
    }

    /// Review VCR-4: whether resets overruled `m`'s votes or caps in `STRIKES_OUT` of the last
    /// `STRIKE_EPOCHS` epochs.
    pub(super) fn struck_out(&self, m: MemberId) -> bool {
        let from = self.vcm.epoch.saturating_sub(STRIKE_EPOCHS);
        self.vcm.strikes.get(&m).is_some_and(|v| v.iter().filter(|&&e| e > from).count() >= STRIKES_OUT)
    }

    /// D80: whether a change needs its boundary ahead of the present, with precommit caps: it adds
    /// members, or it leaves the session without an admin (it closes it at the boundary).
    pub(super) fn change_leads(&self, c: &Change) -> bool {
        !c.add.is_empty() || self.change_closes(c)
    }

    /// D80: whether applying `c` leaves no admin among the members.
    pub(super) fn change_closes(&self, c: &Change) -> bool {
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
    pub(super) fn note_proposal(&mut self, id: EntryId) {
        let e = &self.entries[&id];
        let Payload::Propose { epoch, round, ref change, .. } = e.payload else { return };
        // V3 (review V3-1): a closed session decides nothing more; a former member's proposal is inert.
        if self.vcm.closed_at.is_some() {
            return;
        }
        let v = change.value();
        self.vcm.proposal_ids.entry(v).or_insert(id);
        if epoch > self.vcm.epoch && self.fixes_off & fix::PF1 == 0 {
            self.later_proposals.entry(epoch).or_default().push(id);
        }
        if epoch == self.vcm.epoch && change.epoch == epoch && e.member == self.proposer(epoch, round) {
            let c = change.clone();
            self.vcm.vc.changes.entry(v).or_insert(c);
        }
    }

    /// The proposer of `round` of the view change that ends `epoch`: a seeded rotation over the
    /// epoch's members, so no member can choose to propose.
    pub(super) fn proposer(&self, epoch: u32, round: u32) -> MemberId {
        let mut order: Vec<MemberId> = self.vcm.members.iter().copied().collect();
        let salt = self.cfg.seed ^ ((self.world_id as u64) << 40) ^ ((epoch as u64) << 8);
        order.sort_by_key(|&m| (splitmix64(salt ^ (m as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)), m));
        // A closed session has no members (V3; review V3-1): no one proposes.
        if order.is_empty() {
            return LOGIC;
        }
        order[round as usize % order.len()]
    }

    /// Members this replica holds a proof against.
    pub(super) fn proven_set(&self) -> BTreeSet<MemberId> {
        self.vcm.proofs.keys().chain(self.vcm.liars.keys()).copied().filter(|m| self.vcm.members.contains(m)).collect()
    }

    pub(super) fn proof_of(&self, m: MemberId) -> Proof {
        match self.vcm.proofs.get(&m) {
            Some(&(_, a, b)) => Proof { a, b },
            None => self.vcm.liars[&m],
        }
    }

    /// This epoch's members outside `excluded`, and (t, q) over them.
    pub(super) fn base(&self, excluded: &BTreeSet<MemberId>) -> (BTreeSet<MemberId>, usize, usize) {
        let b: BTreeSet<MemberId> = self.vcm.members.iter().copied().filter(|m| !excluded.contains(m)).collect();
        let (t, q) = quorum(self.cfg.quorum, b.len());
        (b, t, q)
    }

    /// The members a change excludes from its quorums.
    pub(super) fn excluded_of(&self, v: u64) -> Option<BTreeSet<MemberId>> {
        self.vcm.vc.changes.get(&v).map(|c| c.excluded.iter().map(|(m, _)| *m).collect())
    }

    /// Whether a quorum (counted as the change `v` defines it) sent `v` in `round` of `votes`.
    /// Review VC4R-1: whether prevotes for `v` in `round`, in any version held, come from a quorum: the
    /// test of a cap's justification, which an equivocated prevote must not defeat. (Locks and decisions
    /// still use `quorum_for`, one version per member.)
    pub(super) fn polka_any(&self, round: u32, v: u64) -> bool {
        let Some(xs) = self.excluded_of(v) else { return false };
        let (base, _, q) = self.base(&xs);
        self.vcm.vc.any_prevotes.get(&(round, v)).is_some_and(|ms| ms.iter().filter(|m| base.contains(m)).count() >= q)
    }

    /// Whether capper `m`'s polka for `v` in `round` holds here: the prevotes recorded for it in any
    /// version, and (review VC7R-2) every prevote the cap's precommit named that is held, linked into its
    /// author's chain or not. A fetched prevote sitting past a gap in its author's chain counted for
    /// nothing, so the capper was voted out at once.
    pub(super) fn cap_polka_holds(&self, m: MemberId, round: u32, v: u64) -> bool {
        if self.fixes_off & fix::VC7R2 != 0 {
            return self.polka_any(round, v);
        }
        let Some(xs) = self.excluded_of(v) else { return false };
        let (base, _, q) = self.base(&xs);
        let mut ms: BTreeSet<MemberId> = self.vcm.vc.any_prevotes.get(&(round, v)).cloned().unwrap_or_default();
        for id in self.cap_polka.get(&m).into_iter().flatten() {
            if let Some(e) = self.entries.get(id) {
                if matches!(e.payload, Payload::Prevote { epoch, round: r, value } if epoch == self.vcm.epoch && r == round && value == v) {
                    ms.insert(e.member);
                }
            }
        }
        ms.iter().filter(|x| base.contains(x)).count() >= q
    }

    pub(super) fn quorum_for(&self, votes: &BTreeMap<u32, BTreeMap<MemberId, u64>>, round: u32, v: u64) -> bool {
        let Some(xs) = self.excluded_of(v) else { return false };
        let (base, _, q) = self.base(&xs);
        votes.get(&round).is_some_and(|r| r.iter().filter(|&(m, &x)| x == v && base.contains(m)).count() >= q)
    }

    /// The members whose precommit for `v` in `round` counts toward deciding it: those recorded in
    /// chain order, and (review PF-1) every member proven to have forked its chain that signed one
    /// held here, wherever it sits in that chain. A replica keeps the first version of a seq it
    /// receives, so a forker's chain past its fork may never link here; its precommit in a quorum
    /// others decided on would hold this replica in the old epoch for good. Any version counts: a
    /// faulty member's precommit counts once per value, as Tendermint's commit rule counts signatures.
    pub(super) fn precommitters(&self, round: u32, v: u64) -> BTreeSet<MemberId> {
        let mut ms: BTreeSet<MemberId> = self.vcm.vc.precommits.get(&round).map(|r| r.iter().filter(|&(_, &x)| x == v).map(|(&m, _)| m).collect()).unwrap_or_default();
        if self.fixes_off & fix::PF1 == 0 {
            if let Some(held) = self.pc_held.get(&(self.vcm.epoch, round, v)) {
                ms.extend(held.keys().filter(|m| self.vcm.proofs.contains_key(m)));
            }
        }
        ms
    }

    /// The members whose prevote for `v` in `round` counts toward a polka (a lock, a re-proposal's
    /// valid round): those recorded in chain order, and (review VC8R-1) every member proven to have
    /// forked its chain that signed one held here, wherever it sits in that chain, in any version, as
    /// `precommitters` counts precommits. A faulty member's prevote counts once per value; two polkas
    /// for different values of one round still need an honest member that prevoted both.
    pub(super) fn prevoters(&self, round: u32, v: u64) -> BTreeSet<MemberId> {
        let mut ms: BTreeSet<MemberId> = self.vcm.vc.prevotes.get(&round).map(|r| r.iter().filter(|&(_, &x)| x == v).map(|(&m, _)| m).collect()).unwrap_or_default();
        if self.fixes_off & fix::VC8R1 == 0 {
            if let Some(held) = self.pv_held.get(&(self.vcm.epoch, round, v)) {
                ms.extend(held.iter().filter(|m| self.vcm.proofs.contains_key(m)));
            }
        }
        ms
    }

    /// Whether a quorum (counted as the change `v` defines it) prevoted `v` in `round`.
    pub(super) fn polka(&self, round: u32, v: u64) -> bool {
        let Some(xs) = self.excluded_of(v) else { return false };
        let (base, _, q) = self.base(&xs);
        self.prevoters(round, v).iter().filter(|m| base.contains(m)).count() >= q
    }

    pub(super) fn precommit_quorum(&self, round: u32, v: u64) -> bool {
        let Some(xs) = self.excluded_of(v) else { return false };
        let (base, _, q) = self.base(&xs);
        self.precommitters(round, v).iter().filter(|m| base.contains(m)).count() >= q
    }

    /// Members of `base` that sent anything (`nil_only`: nil) in `round` of `votes`.
    pub(super) fn count_round(votes: &BTreeMap<u32, BTreeMap<MemberId, u64>>, round: u32, base: &BTreeSet<MemberId>, nil_only: bool) -> usize {
        votes.get(&round).map_or(0, |r| r.iter().filter(|&(m, &x)| base.contains(m) && (!nil_only || x == 0)).count())
    }

    /// The deepest point of `x`'s chain that at least `k` of the given frozen heads extend
    /// (design 10.8). With at most t faulty voters among them and `k` = their number minus t,
    /// every honest head extends it, so it keeps every entry of `x` stable anywhere; and since
    /// they number more than 2t, two branches can never both reach `k`. None while an entry
    /// needed to walk back is missing (it is fetched from the voter).
    pub(super) fn robust_cut(&mut self, x: MemberId, heads: &[(MemberId, (u32, EntryId))], k: usize) -> Option<(u32, EntryId)> {
        let mut cur: Vec<(MemberId, u32, EntryId)> = heads.iter().map(|(v, (s, id))| (*v, *s, *id)).collect();
        let mut c = cur.iter().map(|h| h.1).max().unwrap_or(0);
        loop {
            if c == 0 || k == 0 {
                return Some((0, EntryId::NONE));
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
    pub(super) fn verify_proof(&mut self, m: MemberId, p: Proof) -> Option<bool> {
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
    pub(super) fn build_change(&mut self) -> Option<Change> {
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
        let mut order: Vec<((u64, MemberId, u32), EntryId)> =
            self.vcm.admin_pending.iter().filter_map(|id| self.entries.get(id).map(|e| (admin_key(e), *id))).collect();
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
        let targets: Vec<MemberId> = self.vcm.votes.keys().copied().filter(|x| run.members.contains(x)).collect();
        // Review VC3R-1/2 (round 4): a removal a quorum has voted for, which waits only for entries this
        // replica is still fetching, must not be overtaken by a reset.
        let mut waiting = false;
        for x in targets {
            // Review VC4R-5: a vote whose frozen head is still unresolved half a window after it was
            // recorded counts for nothing here, so one faulty vote cannot hold every reset back.
            let fresh = |r: &VoteRec| self.fixes_off & fix::VC4R5 != 0 || self.now < r.at + self.cfg.window / 2 || self.head_held(x, r.frozen);
            let raw = self.vcm.votes.get(&x).map_or(0, |v| v.iter().filter(|(m, r)| base.contains(m) && r.kick == EntryId::NONE && fresh(r)).count());
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
            remove.push(Removal { member: x, votes: usable.iter().map(|(_, r)| r.id).collect(), cut, kick: EntryId::NONE });
            run.members.remove(&x);
            run.roles.remove(&x);
        }
        // A join's boundary is about now plus a join lead: a request expiring before it is not proposed.
        let lead_at = (self.now + self.cfg.join_lead).max(self.vcm.decided.last().map_or(0, |d| d.0.boundary));
        let carry = self.fixes_off & fix::VC4R2 == 0;
        let joins: Vec<(MemberId, RoleId, Vec<Attest>, (Sig, u64))> = self
            .vcm.join_requests
            .iter()
            .copied()
            .filter(|j| !self.vcm.members.contains(j) && !self.vcm.evicted.contains_key(j))
            .filter_map(|j| {
                let req = self.join_reqs.get(&j).copied().unwrap_or(([0; 64], u64::MAX));
                if carry && req.1 < lead_at {
                    return None;
                }
                let attests = self.vcm.join_attests.get(&j).cloned().unwrap_or_default();
                self.join_role(j, &attests, &run).map(|r| (j, r, attests, req))
            })
            .collect();
        let mut add = Vec::new();
        let mut add_roles = Vec::new();
        let mut add_attests = Vec::new();
        let mut add_reqs = Vec::new();
        let mut join_keys = BTreeSet::new();
        for (j, r, a, req) in joins {
            // 12.4 (review BN-3): one key, one member, among the change's joiners too.
            if self.key_of(j).is_some_and(|k| !join_keys.insert(k)) {
                continue;
            }
            add.push(j);
            add_roles.push(r);
            add_attests.push(if self.fixes_off & fix::VCR7 == 0 { a } else { Vec::new() });
            if carry {
                add_reqs.push(req);
            }
        }
        let last = self.vcm.decided.last().map_or(0, |d| d.0.boundary);
        let mut c = Change { epoch: self.vcm.epoch, remove, add, boundary: 0, excluded, admin, add_roles, add_attests, add_reqs };
        c.boundary = if self.change_leads(&c) { (self.now + self.cfg.join_lead).max(last) } else { self.now.max(last) };
        if c.remove.is_empty() && c.add.is_empty() && c.admin.is_empty() {
            // A reset answers a suspicion vote against a member heard from again, never a kick vote;
            // and caps from precommits whose change this replica cannot propose (review VC-4).
            let now = self.now;
            // Review VC3R-2: stale caps and ghost kick votes are no longer answered by a reset (round 3)
            // but by voting their authors out (`suspects_due`): a reset keeps the member that forced it.
            let round3 = self.fixes_off & fix::VC3R2 != 0;
            let stale_caps = round3 && self.fixes_off & fix::VC4 == 0 && !self.vcm.join_caps.is_empty();
            // Review VCR-1: a kick vote naming no held, pending kick of its target, for as long as a
            // silent member is given, is answered like a suspicion vote.
            let ghosts = round3 && self.fixes_off & fix::VCR1 == 0;
            let reset = stale_caps || self.vcm.votes.iter().any(|(y, vs)| {
                let heard = self.heard.get(y).copied().unwrap_or(0);
                !proven.contains(y)
                    && now < heard + self.cfg.suspect_after
                    && vs.values().any(|r| r.kick == EntryId::NONE || (ghosts && !self.kick_live(*y, r.kick) && now >= r.at + self.cfg.suspect_after))
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
                let ats: Vec<u64> = self.vcm.votes.values().flat_map(|vs| vs.values().map(|r| r.at)).collect();
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
    pub(super) fn valid_on_own_branch(&mut self, id: EntryId) -> Option<bool> {
        let e = self.entries.get(&id)?.clone();
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
        let mut prev_epoch = self.vcm.ack_epoch_from.get(&m).and_then(|a| a.iter().filter(|&(_, &sq)| sq <= k).map(|(&ep, _)| ep).max()).unwrap_or(0);
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

    pub(super) fn change_valid(&mut self, c: &Change) -> Option<bool> {
        let last = self.vcm.decided.last().map_or(0, |d| d.0.boundary);
        let timed = self.fixes_off & fix::VC3 != 0;
        // Review P23-10: member id 0 is world logic's, never a member's.
        if c.epoch != self.vcm.epoch || c.boundary < last || (timed && c.boundary > self.now + 2 * self.cfg.join_lead) || c.add.contains(&LOGIC) {
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
            if self.vcm.members.contains(&j) || self.vcm.evicted.contains_key(&j) || !joiners.insert(j) {
                return Some(false);
            }
            // Review VC4R-2: the change carries the joiner's signed request, so every validator judges
            // it alike; waiting for a copy of its own let a request that reached only some members wedge
            // the members locked on the join.
            if carried {
                let (sig, expires) = c.add_reqs[i];
                let ok = self.join_sig_ok(j, &sig, expires);
                if !ok || c.boundary > expires {
                    return Some(false);
                }
            } else if !self.vcm.join_requests.contains(&j) {
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
            if !self.vcm.members.contains(&m) || !xs.insert(m) || !self.verify_proof(m, p)? {
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
                Payload::Kick { epoch, .. } | Payload::SetRole { epoch, .. } | Payload::DefineRole { epoch, .. } | Payload::Ownership { epoch, .. } | Payload::Ban { epoch, .. } => epoch,
                _ => return Some(false),
            };
            let key = admin_key(&e);
            if epoch > self.vcm.epoch || self.vcm.admin_outcomes.contains_key(&id) || prev.is_some_and(|p| p >= key) || run.apply(&e) != ok {
                return Some(false);
            }
            prev = Some(key);
            if let (true, Payload::Kick { target, .. }) = (ok, &e.payload) {
                kicked.insert(id, *target);
            }
        }
        let mut seen = BTreeSet::new();
        for r in &c.remove {
            let x = r.member;
            if !self.vcm.members.contains(&x) || !seen.insert(x) || r.votes.len() < q {
                return Some(false);
            }
            // A kick's removal needs its kick applied; a removal on suspicion or proof needs a member
            // no kick removed (D80).
            if r.kick != EntryId::NONE && kicked.get(&r.kick) != Some(&x) || r.kick == EntryId::NONE && !run.members.contains(&x) {
                return Some(false);
            }
            let mut voters = BTreeSet::new();
            let mut heads = Vec::new();
            for &vid in &r.votes {
                let e = self.entries.get(&vid)?.clone();
                let Payload::Vote { epoch, target, frozen, kick, .. } = e.payload else { return Some(false) };
                if epoch != self.vcm.epoch || target != x || !base.contains(&e.member) || !voters.insert(e.member) || (r.kick == EntryId::NONE && kick != EntryId::NONE) {
                    return Some(false);
                }
                // Review VCR-2: a vote on one's own kick freezes exactly the entry before it.
                if e.member == x && self.fixes_off & fix::VCR2 == 0 && frozen != (e.seq - 1, e.prev) {
                    return Some(false);
                }
                // The voter's first vote against x in this epoch, in the copy of its chain I hold.
                match self.vcm.votes.get(&x).and_then(|v| v.get(&e.member)) {
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
            if r.kick == EntryId::NONE {
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
        let mut join_keys = BTreeSet::new();
        for ((&j, &role), carried) in c.add.iter().zip(&c.add_roles).zip(&c.add_attests) {
            if self.key_of(j).is_some_and(|k| !join_keys.insert(k)) {
                return Some(false);
            }
            // Review VCR-7: the change's own attestations decide, the same at every replica.
            let attests = if self.fixes_off & fix::VCR7 == 0 { carried.clone() } else { self.vcm.join_attests.get(&j).cloned().unwrap_or_default() };
            if self.join_role(j, &attests, &run) != Some(role) {
                return Some(false);
            }
        }
        Some(true)
    }

    pub(super) fn author_vc(&mut self, payload: Payload) -> Entry {
        let e = Entry { member: self.me, seq: self.my_seq + 1, prev: self.my_last, stamp: self.frame.max(self.now), payload, carried: None };
        self.publish(e)
    }

    /// A precommit, signed over its fact for the epoch certificate (S3c).
    pub(super) fn author_precommit(&mut self, epoch: u32, round: u32, value: u64, boundary: u64, polka: Vec<EntryId>) -> Entry {
        let sig = self.keys.sign_fact(self.world_id, vc_fact(epoch, round, value));
        self.author_vc(Payload::Precommit { epoch, round, value, boundary, polka, sig })
    }

    pub(super) fn start_round(&mut self, r: u32) {
        self.vcm.vc.round = r;
        self.vcm.vc.step = Step::Propose;
        self.vcm.vc.round_at = self.now;
        self.vcm.vc.proposed = false;
        self.stats.max_round = self.stats.max_round.max(r);
    }

    pub(super) fn prevote(&mut self, v: u64, out: &mut Vec<Entry>) {
        self.vcm.vc.step = Step::Prevote;
        if !self.vcm.withhold_vc {
            let (epoch, round) = (self.vcm.epoch, self.vcm.vc.round);
            out.push(self.author_vc(Payload::Prevote { epoch, round, value: v }));
        }
    }

    pub(super) fn precommit(&mut self, v: u64, out: &mut Vec<Entry>) {
        self.precommit_join(v, 0, out);
    }

    pub(super) fn precommit_join(&mut self, v: u64, boundary: u64, out: &mut Vec<Entry>) {
        self.vcm.vc.step = Step::Precommit;
        if !self.vcm.withhold_vc {
            let (epoch, round) = (self.vcm.epoch, self.vcm.vc.round);
            // Review VC4R-1: a capped precommit names the prevote ids of the polka this replica saw for
            // `v` in `round`, so a member that lacks one (an equivocated prevote sent only to me) can
            // fetch it before it holds my cap unjustified.
            // Review VC6R-2: at most a quorum of prevote ids; more is never needed to justify the cap,
            // and listing every prevoting member overran the encoding in a large session. Capped at
            // MAX_POLKA as well (review 6.1 low): a quorum passes MAX_POLKA only past ~768 members (3x
            // the 256-player target), where the proof is truncated rather than overrunning the encoding.
            let polka = if boundary > 0 {
                let (_, q) = quorum(self.cfg.quorum, self.vcm.members.len());
                let mut ids: Vec<EntryId> = self
                    .vcm.vc
                    .prevotes
                    .get(&round)
                    .map(|ms| ms.iter().filter(|&(_, &x)| x == v).filter_map(|(k, _)| self.vcm.vc.prevote_id.get(&(round, *k)).copied()).collect())
                    .unwrap_or_default();
                ids.truncate(q.min(MAX_POLKA));
                ids
            } else {
                Vec::new()
            };
            out.push(self.author_precommit(epoch, round, v, boundary, polka));
        }
    }

    /// Run this epoch's view change and return the entries to broadcast. Called every tick:
    /// Tendermint's rules (algorithm 1, lines 11-65) over chain entries, timeouts in clock frames.
    pub fn view_change_due(&mut self) -> Vec<SignedEntry> {
        if self.is_static() {
            return Vec::new();
        }
        let out = self.view_change_due_entries();
        self.signed_all(out)
    }

    fn view_change_due_entries(&mut self) -> Vec<Entry> {
        let mut out = Vec::new();
        if self.vcm.closed_at.is_some() {
            return out;
        }
        // Everyone that holds the log follows its decisions, members or not (a joiner learns so).
        if !self.vcm.byz_ally && self.try_decide(&mut out) {
            return out;
        }
        if !self.vcm.members.contains(&self.me) {
            return out;
        }
        if self.vcm.byz_ally {
            // Prevote and precommit every proposal seen, first per round (a linear chain).
            let props: Vec<(u32, u64)> = self.vcm.vc.proposals.iter().map(|(r, (v, _))| (*r, *v)).collect();
            for (r, v) in props {
                let me = self.me;
                if !self.vcm.vc.prevotes.get(&r).is_some_and(|x| x.contains_key(&me)) {
                    let epoch = self.vcm.epoch;
                    out.push(self.author_vc(Payload::Prevote { epoch, round: r, value: v }));
                    if !self.vcm.byz_no_precommit {
                        let boundary = self.vcm.vc.changes.get(&v).filter(|c| self.change_leads(c)).map_or(0, |c| c.boundary);
                        out.push(self.author_precommit(epoch, r, v, boundary, vec![]));
                    }
                }
            }
            // Drive every round another member gave up on: echo its nil prevote or precommit where no
            // proposal is held, so a side of a partition that is no quorum alone still reaches a round
            // one of its own members proposes in.
            let (me, epoch) = (self.me, self.vcm.epoch);
            let nil_in = |votes: &BTreeMap<u32, BTreeMap<MemberId, u64>>| -> Vec<u32> {
                votes.iter().filter(|(_, ms)| !ms.contains_key(&me) && ms.values().any(|v| *v == 0)).map(|(r, _)| *r).collect()
            };
            for r in nil_in(&self.vcm.vc.prevotes) {
                if !self.vcm.vc.proposals.contains_key(&r) {
                    out.push(self.author_vc(Payload::Prevote { epoch, round: r, value: 0 }));
                }
            }
            for r in nil_in(&self.vcm.vc.precommits) {
                if !self.vcm.vc.proposals.contains_key(&r) && !self.vcm.byz_no_precommit {
                    out.push(self.author_precommit(epoch, r, 0, 0, vec![]));
                }
            }
            return out;
        }
        if self.vcm.vc.since.is_none() {
            let joins = self.vcm.join_requests.iter().any(|j| !self.vcm.members.contains(j) && !self.vcm.evicted.contains_key(j));
            let busy = joins
                || (self.fixes_off & fix::VC4 == 0 && !self.vcm.join_caps.is_empty())
                || !self.vcm.votes.is_empty()
                || !self.vcm.admin_pending.is_empty()
                || !self.vcm.vc.proposals.is_empty()
                || !self.vcm.vc.prevotes.is_empty()
                || !self.vcm.vc.precommits.is_empty();
            if !busy {
                return out;
            }
            self.vcm.vc.since = Some(self.now);
            self.start_round(0);
        }
        for _ in 0..16 {
            let before = (self.vcm.vc.round, self.vcm.vc.step, self.vcm.vc.proposed, self.vcm.vc.polka_done.len(), self.vcm.vc.prevote_timer.len(), self.vcm.vc.precommit_timer.len());
            self.vc_rules(&mut out);
            if self.try_decide(&mut out) {
                return out;
            }
            if before == (self.vcm.vc.round, self.vcm.vc.step, self.vcm.vc.proposed, self.vcm.vc.polka_done.len(), self.vcm.vc.prevote_timer.len(), self.vcm.vc.precommit_timer.len()) {
                break;
            }
        }
        out
    }

    pub(super) fn vc_rules(&mut self, out: &mut Vec<Entry>) {
        let (e, r) = (self.vcm.epoch, self.vcm.vc.round);
        let proven = self.proven_set();
        let (any_base, t_any, q_any) = self.base(&proven);
        // Lines 55-56: more than t members are in a later round: go there.
        let mut hi: BTreeMap<MemberId, u32> = BTreeMap::new();
        for votes in [&self.vcm.vc.prevotes, &self.vcm.vc.precommits] {
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
        if self.vcm.vc.step == Step::Propose && !self.vcm.vc.proposed && !self.vcm.withhold_vc && self.proposer(e, r) == self.me {
            let byz = self.byz_change.clone();
            let (vr, change) = match (self.vcm.vc.valid, byz) {
                (_, Some(c)) => (NO_ROUND, Some(Change { epoch: e, boundary: c.boundary.max(self.now), ..c })),
                (Some((v, vr)), None) => (vr, self.vcm.vc.changes.get(&v).cloned()),
                (None, None) => (NO_ROUND, self.build_change()),
            };
            // Members that fail together are suspected within a few frames of each other: give the
            // votes on every member this replica itself suspects a few frames to arrive, so that one
            // change removes them all.
            let me = self.me;
            let gathering = vr == NO_ROUND
                && self.now < self.vcm.vc.round_at + VC_GATHER
                && change.as_ref().is_some_and(|c| self.vcm.votes.iter().any(|(y, vs)| vs.contains_key(&me) && !c.remove.iter().any(|x| x.member == *y)));
            if let Some(c) = change.filter(|_| !gathering) {
                self.vcm.vc.proposed = true;
                self.ev(format!("view change: propose in round {r}: remove {:?}, excluded {:?}", c.remove.iter().map(|x| (x.member, x.cut.0)).collect::<Vec<_>>(), c.excluded.iter().map(|x| x.0).collect::<Vec<_>>()));
                out.push(self.author_vc(Payload::Propose { epoch: e, round: r, valid_round: vr, change: c }));
            }
        }
        // Lines 22-33: prevote on the round's proposal.
        if self.vcm.vc.step == Step::Propose {
            if let Some(&(v, vr)) = self.vcm.vc.proposals.get(&r) {
                let c = self.vcm.vc.changes.get(&v).cloned();
                let ok = match c {
                    Some(c) => self.change_valid(&c),
                    None => Some(false),
                };
                // Review VC-3: validity never changes with time (a Tendermint lock relies on it); a
                // fresh proposal must also bring its boundary in time, a re-proposal need not.
                let fresh_in_time = match self.vcm.vc.changes.get(&v) {
                    Some(c) => {
                        let last = self.vcm.decided.last().map_or(0, |d| d.0.boundary);
                        let leads = self.change_leads(c);
                        c.boundary >= last && c.boundary <= self.now + 2 * self.cfg.join_lead && (!leads || c.boundary > self.now + 1)
                    }
                    None => false,
                };
                let decision = match ok {
                    None => None,
                    Some(ok) if vr == NO_ROUND => Some(ok && fresh_in_time && self.vcm.vc.locked.is_none_or(|(lv, _)| lv == v)),
                    Some(ok) if vr < r && self.polka(vr, v) => {
                        Some(ok && self.vcm.vc.locked.is_none_or(|(lv, lr)| lr <= vr || lv == v))
                    }
                    Some(_) => None,
                };
                if let Some(yes) = decision {
                    self.prevote(if yes { v } else { 0 }, out);
                }
            }
        }
        // Lines 34-35: a quorum of prevotes of any kind arms the prevote timeout.
        if self.vcm.vc.step == Step::Prevote && !self.vcm.vc.prevote_timer.contains_key(&r) && Self::count_round(&self.vcm.vc.prevotes, r, &any_base, false) >= q_any {
            self.vcm.vc.prevote_timer.insert(r, self.now);
        }
        // Lines 36-43: a quorum prevoted the round's proposal: lock it and precommit it.
        if self.vcm.vc.step != Step::Propose && !self.vcm.vc.polka_done.contains(&r) {
            if let Some(&(v, _)) = self.vcm.vc.proposals.get(&r) {
                if self.polka(r, v) {
                    let c = self.vcm.vc.changes.get(&v).cloned().unwrap_or_default();
                    let leads = self.change_leads(&c);
                    // My precommit caps my acks below the boundary only while it is still ahead of
                    // me. Past it, I may still precommit (without a cap) once some member's acks are
                    // capped at or below it: readers then confirm nothing from the boundary on until
                    // they reach the next epoch, whoever that member is (review VC-3).
                    let ahead = c.boundary > self.now;
                    let capped = self.vcm.join_caps.values().any(|&(_, b)| b <= c.boundary);
                    let in_time = !leads || ahead || (self.fixes_off & fix::VC3 == 0 && capped);
                    if self.vcm.vc.changes.contains_key(&v) && in_time && self.change_valid(&c) == Some(true) {
                        self.vcm.vc.polka_done.insert(r);
                        if self.vcm.vc.step == Step::Prevote {
                            self.vcm.vc.locked = Some((v, r));
                            self.precommit_join(v, if leads && ahead { c.boundary } else { 0 }, out);
                        }
                        self.vcm.vc.valid = Some((v, r));
                    }
                }
            }
        }
        // Lines 44-46: a quorum prevoted nil.
        if self.vcm.vc.step == Step::Prevote && Self::count_round(&self.vcm.vc.prevotes, r, &any_base, true) >= q_any {
            self.precommit(0, out);
        }
        // Lines 47-48: a quorum of precommits of any kind arms the precommit timeout; a quorum of
        // nil precommits ends the round at once (no value can be decided in it).
        if Self::count_round(&self.vcm.vc.precommits, r, &any_base, true) >= q_any {
            self.start_round(r + 1);
            return;
        }
        if !self.vcm.vc.precommit_timer.contains_key(&r) && Self::count_round(&self.vcm.vc.precommits, r, &any_base, false) >= q_any {
            self.vcm.vc.precommit_timer.insert(r, self.now);
        }
        // Lines 57-65: timeouts, lengthening with the round.
        let to = self.cfg.vc_timeout + self.cfg.vc_timeout_step * r as u64;
        if self.vcm.vc.step == Step::Propose && self.now >= self.vcm.vc.round_at + to {
            self.prevote(0, out);
        }
        if self.vcm.vc.step == Step::Prevote && self.vcm.vc.prevote_timer.get(&r).is_some_and(|&a| self.now >= a + to) {
            self.precommit(0, out);
        }
        if self.vcm.vc.precommit_timer.get(&r).is_some_and(|&a| self.now >= a + to) {
            self.start_round(r + 1);
        }
        let _ = t_any;
    }

    /// Lines 49-54: a quorum precommitted a value in some round: decide it and apply it. A value
    /// this replica holds no proposal for is fetched from those that precommitted it.
    pub(super) fn try_decide(&mut self, out: &mut Vec<Entry>) -> bool {
        let mut candidates = Vec::new();
        let mut unknown = Vec::new();
        for (&rr, ms) in &self.vcm.vc.precommits {
            let mut vals: BTreeMap<u64, Vec<MemberId>> = BTreeMap::new();
            for (&m, &v) in ms {
                if v != 0 {
                    vals.entry(v).or_default().push(m);
                }
            }
            for (v, ms) in vals {
                if !self.vcm.vc.changes.contains_key(&v) {
                    unknown.push((v, ms));
                } else if self.precommit_quorum(rr, v) {
                    candidates.push((rr, v));
                }
            }
        }
        for (v, ms) in unknown {
            for m in ms {
                if m != self.me && self.vcm.value_asked.insert((m, v)) {
                    self.vcm.value_wants.push((m, v));
                }
            }
        }
        for (rr, v) in candidates {
            // A change's exclusions set its quorum: decide it only once every one is proved, or a
            // faulty minority could exclude honest members and decide alone (P2-4).
            let ex = self.vcm.vc.changes[&v].excluded.clone();
            let mut proved = true;
            for (m, p) in ex {
                if self.verify_proof(m, p) != Some(true) {
                    proved = false;
                }
            }
            // Review VCR-8: applying a change runs its admin entries, so every one must be held first;
            // ask those that precommitted it for the ones missing.
            if proved && self.fixes_off & fix::VCR8 == 0 {
                let missing: Vec<EntryId> = self.vcm.vc.changes[&v].admin.iter().map(|(id, _)| *id).filter(|id| !self.entries.contains_key(id)).collect();
                if !missing.is_empty() {
                    let askers: Vec<MemberId> = self.vcm.vc.precommits.get(&rr).map(|ms| ms.iter().filter(|&(_, &x)| x == v).map(|(&m, _)| m).collect()).unwrap_or_default();
                    for id in missing {
                        for &m in &askers {
                            self.want_entry(m, id, 0);
                        }
                    }
                    continue;
                }
            }
            if proved {
                let c = self.vcm.vc.changes[&v].clone();
                self.apply_change(c, rr, out);
                return true;
            }
        }
        false
    }

    /// Apply a decided change: the next epoch's members, each removed member's cut, a new epoch
    /// with no votes, and the epoch certificate's material.
    pub(super) fn apply_change(&mut self, c: Change, round: u32, out: &mut Vec<Entry>) {
        let v = c.value();
        let xs: BTreeSet<MemberId> = c.excluded.iter().map(|(m, _)| *m).collect();
        // Review VC7R-5: exactly a quorum of signers, the lowest member ids among the precommits held, so
        // the certificate's size is bounded and replicas holding more than a quorum agree on it. Two
        // replicas that decided on different quorums still sign different (equally valid) certificates:
        // a certificate is checked by its signatures (`epoch_cert_ok`), never compared by its bytes.
        // PF-1: a proven forker's precommit counts by its signature, held at insert (`pc_held`).
        let (_, _, q_sig) = self.base(&xs);
        let held = self.pc_held.get(&(self.vcm.epoch, round, v));
        let sigs: Vec<(MemberId, Sig)> = self
            .precommitters(round, v)
            .into_iter()
            .filter(|m| !xs.contains(m))
            .take(q_sig)
            .map(|m| (m, *self.vcm.vc.precommit_sigs.get(&(round, v, m)).or_else(|| held.and_then(|h| h.get(&m))).expect("a counted precommit's signature is held")))
            .collect();
        let removed: Vec<MemberId> = c.remove.iter().map(|r| r.member).collect();
        // D80: role entries in commit order (kicks are among the removals), then the removals, then
        // each joiner's starting role against the membership they leave.
        let closes = self.change_closes(&c);
        for &(id, ok) in &c.admin {
            self.vcm.admin_outcomes.insert(id, ok);
            if ok {
                self.stats.admin_applied += 1;
                // Review VCR-8: with the fix, every listed entry is held here (`try_decide`); without
                // it, a missing one panics the replica, as it did before.
                let payload = if self.fixes_off & fix::VCR8 != 0 { Some(self.entries[&id].payload.clone()) } else { self.entries.get(&id).map(|e| e.payload.clone()) };
                match payload {
                    Some(Payload::SetRole { member, role, .. }) => {
                        self.vcm.roles.insert(member, role);
                        // 12.4: as in the run, an explicit promotion lifts the member's rejoin role.
                        if (role == ADMIN || self.vcm.role_defs.get(&role).is_some_and(|p| p & perm::ROLES != 0))
                            && let Some(k) = self.key_of(member)
                        {
                            lift_rejoin_role(&mut self.vcm.bans, &k);
                        }
                    }
                    // 12.4: the kick's option, for the target's key (its removal is among the removals).
                    Some(Payload::Kick { target, rejoin, .. }) => {
                        if let Some(k) = self.key_of(target) {
                            restrict(&mut self.vcm.bans, &mut self.vcm.perma_bans, k, rejoin);
                        }
                    }
                    Some(Payload::Ban { key, on, perma, .. }) => {
                        ban(&mut self.vcm.bans, &mut self.vcm.perma_bans, key, on, perma);
                    }
                    Some(Payload::DefineRole { role, perms, .. }) => {
                        self.vcm.role_defs.insert(role, perms);
                    }
                    // 12.5: the session commits the certificate at the deciding boundary. Its
                    // validity does not depend on the frame, so it applies here as in the run.
                    Some(Payload::Ownership { cert, .. }) => {
                        let lineage = self.world_id;
                        if let Some(b) = self.vcm.book.as_mut() {
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
        for r in &c.remove {
            self.vcm.members.remove(&r.member);
            self.vcm.roles.remove(&r.member);
            self.vcm.frozen.insert(r.member, r.cut.0);
            self.vcm.cut_ids.insert(r.member, r.cut.1);
            self.vcm.evicted.insert(r.member, (r.cut.0, 0));
        }
        for (&j, &role) in c.add.iter().zip(&c.add_roles) {
            self.vcm.roles.insert(j, role);
        }
        for &j in &c.add {
            self.vcm.members.insert(j);
            self.known.insert(j);
            self.vcm.admitted.insert(j, c.boundary);
            self.vcm.followers.remove(&j);
            self.vcm.join_requests.remove(&j);
            self.vcm.join_attests.remove(&j);
            self.join_reqs.remove(&j);
        }
        for (id, _) in &c.admin {
            self.vcm.admin_pending.remove(id);
        }
        // A pending entry about a member no longer here can never apply: drop it unjudged.
        let gone: Vec<EntryId> = self
            .vcm.admin_pending
            .iter()
            .copied()
            .filter(|id| match self.entries.get(id).map(|e| &e.payload) {
                Some(Payload::Kick { target: x, .. }) | Some(Payload::SetRole { member: x, .. }) => !self.vcm.members.contains(x),
                _ => false,
            })
            .collect();
        for id in gone {
            self.vcm.admin_pending.remove(&id);
        }
        // 12.5: a pending certificate the decision made stale or invalid can never apply: drop it
        // unjudged, as `gone` does (V1's receipt rule).
        let dead: Vec<EntryId> = self
            .vcm.admin_pending
            .iter()
            .copied()
            .filter(|id| matches!(self.entries.get(id).map(|e| &e.payload), Some(Payload::Ownership { cert, .. }) if !self.ownership_applies(cert)))
            .collect();
        for id in dead {
            self.vcm.admin_pending.remove(&id);
        }
        // Review OWNY-2: a held certificate whose parents the decision brought into the book is judged
        // again: pending if it now applies, dropped if it never can; still held while a parent is missing.
        let held: Vec<EntryId> = self.vcm.own_held.iter().copied().collect();
        for id in held {
            let Some(e) = self.entries.get(&id) else { continue };
            let Payload::Ownership { cert, .. } = &e.payload else { continue };
            if self.vcm.admin_outcomes.contains_key(&id) || self.vcm.closed_at.is_some() || !self.vcm.members.contains(&e.member) {
                self.vcm.own_held.remove(&id);
                continue;
            }
            if self.vcm.book.as_ref().is_some_and(|b| cert.prev.iter().any(|p| b.cert(*p).is_none())) {
                continue;
            }
            match self.ownership_judge(cert) {
                Err(OwnError::MissingParent) => {}
                r => {
                    self.vcm.own_held.remove(&id);
                    if r == Ok(Insert::Applied) {
                        self.vcm.admin_pending.insert(id);
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
            self.vcm.lagging.clear();
        } else {
            // VC-6: a member that alone sees another as slow must not force a reset the instant each one
            // ends. A decision answered my standing suspicion vote against y if it kept y, its proposal
            // reached me after I voted, and no other member's vote against y stood with mine. Then I hold
            // off voting against y, twice as long each time in a row, up to a silence limit. A vote that
            // others share, or that the decided proposal never weighed (it reached me first), is cast
            // again at once: a member forcing a reset every epoch cannot keep a quorum of honest votes
            // from standing together, and members whose timers run apart still meet.
            let seen = self.vcm.vc.seen_at.get(&v).copied();
            let mine: Vec<(MemberId, bool)> = self
                .vcm.votes
                .iter()
                .filter(|(y, _)| !removed.contains(y))
                .filter_map(|(y, vs)| vs.get(&self.me).filter(|r| r.kick == EntryId::NONE).map(|r| (*y, vs.len() == 1 && seen.is_none_or(|s| r.at <= s))))
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
        if self.fixes_off & fix::VCR4 != 0 {
            self.vcm.newer_since.clear();
        }
        // Review VCR-4: a reset overrules every vote and cap of its epoch: count each against its author,
        // so a member that forces one each epoch is voted out (`STRIKES_OUT`). Votes against a member
        // already struck out are not counted against their voters.
        if self.fixes_off & fix::VCR4 == 0 && self.fixes_off & fix::VC3R2 != 0 && c.remove.is_empty() && c.add.is_empty() && c.admin.is_empty() {
            let mut struck: BTreeSet<MemberId> = self.vcm.join_caps.keys().copied().collect();
            for (y, vs) in &self.vcm.votes {
                if self.struck_out(*y) {
                    continue;
                }
                for (v, r) in vs {
                    if *v != *y && (r.kick == EntryId::NONE || !self.kick_live(*y, r.kick)) {
                        struck.insert(*v);
                    }
                }
            }
            let reached = self.vcm.epoch + 1;
            for m in struck {
                let v = self.vcm.strikes.entry(m).or_default();
                if v.last() != Some(&reached) {
                    v.push(reached);
                }
            }
        }
        // Review VC-9: references to and from a new member count from its admission.
        let now = self.now;
        let all: Vec<MemberId> = self.vcm.members.iter().copied().chain(c.add.iter().copied()).collect();
        for &j in &c.add {
            for &k in &all {
                if k != j {
                    self.vcm.ref_advanced.insert((k, j), now);
                    self.vcm.ref_advanced.insert((j, k), now);
                }
            }
        }
        self.vcm.join_caps.clear();
        self.cap_seen.clear();
        self.cap_polka.clear();
        if self.fixes_off & fix::VC6R4 == 0 {
            // Review VC6R-4: reset only the timers a cleared vote made stale (this decision clears every
            // vote, below). A voter froze its target's chain, so its reference to that chain stood still
            // (`ref_seen`), and the targets of my own votes looked silent or slow to me
            // (`suspect_since`); new members count from admission. Round 6 cleared every timer at every
            // decision, so a member that forces decisions (a no-op admin entry every 2 s) kept its own
            // timers from ever ripening.
            for (y, vs) in &self.vcm.votes {
                for v in vs.keys() {
                    self.ref_seen.remove(&(*v, *y));
                    if *v == self.me {
                        self.suspect_since.remove(y);
                    }
                }
            }
            for &j in &c.add {
                self.ref_seen.retain(|&(k, a), _| k != j && a != j);
                self.suspect_since.remove(&j);
            }
        } else {
            // Review VC5R-4 (round 6): the verified-reference timers and the silence spare-window are
            // per-epoch; a decision clears them so a stale entry does not vote a member out in the new
            // epoch.
            self.ref_seen.clear();
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
        let from = self.vcm.epoch;
        self.vcm.decided.push((c.clone(), round, sigs));
        self.vcm.epoch += 1;
        self.vcm.vc_log.push((self.vcm.epoch, self.now, round, removed.clone()));
        self.stats.view_changes += 1;
        if removed.is_empty() && c.add.is_empty() && c.admin.is_empty() {
            self.stats.resets += 1;
        }
        self.ev(format!(
            "VIEW CHANGE decided (epoch {from} -> {}, round {round}): removed {:?} with cuts {:?}, excluded by proof {:?}, added {:?} from frame {}; members {:?}",
            self.vcm.epoch,
            removed,
            c.remove.iter().map(|r| r.cut.0).collect::<Vec<_>>(),
            xs,
            c.add,
            c.boundary,
            self.vcm.members
        ));
        // A new epoch: the old one's votes, freezes and view-change state end.
        self.vcm.votes.clear();
        self.vcm.vc = Vc::default();
        self.pc_held.retain(|&(e, _, _), _| e >= self.vcm.epoch);
        self.pv_held.retain(|&(e, _, _), _| e >= self.vcm.epoch);
        let unfreeze: Vec<MemberId> = self.vcm.frozen.keys().copied().filter(|m| !self.vcm.evicted.contains_key(m)).collect();
        for m in unfreeze {
            self.vcm.frozen.remove(&m);
        }
        let members: Vec<MemberId> = self.vcm.members.iter().copied().collect();
        for m in members {
            self.extend_chain(m, out);
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
        if closes && self.vcm.closed_at.is_none() {
            self.vcm.closed_at = Some(c.boundary);
            // V3 (user 08:31Z): closing removes every other player, tells everyone, and allows no further
            // change to the world; no longer grace period and no automatic handover.
            // pe-session empties the member list (the spike keeps its own for bookkeeping):
            // `closed_members` records who the close removed, and `SessionCore` tells the players
            // (`Event::Closed`).
            self.closed_members = self.vcm.members.iter().copied().collect();
            self.vcm.members.clear();
            self.ev(format!("SESSION CLOSED (epoch {}): no admin is left; every member {:?} is removed, and nothing stamped at or after frame {} acts", self.vcm.epoch, self.closed_members, c.boundary));
            let late: Vec<EntryId> = self.entries.iter().filter(|(_, e)| e.stamp >= c.boundary && !is_protocol(&e.payload)).map(|(id, _)| *id).collect();
            for id in late {
                self.set_eff(id);
            }
        }
        if let Some(ids) = self.vcm.vc_later.remove(&self.vcm.epoch) {
            for id in ids {
                let m = self.entries[&id].member;
                self.record_vc(m, id, out);
            }
        }
        // Review PF-1: proposals of the new epoch kept by value before it began (another version, or a
        // proven forker's past its fork) name values it may decide. Indexed by epoch (VC7R-6).
        if self.fixes_off & fix::PF1 == 0 {
            let mut keep = self.later_proposals.split_off(&self.vcm.epoch);
            let held = keep.remove(&self.vcm.epoch).unwrap_or_default();
            self.later_proposals = keep;
            for id in held {
                self.note_proposal(id);
            }
        }
        self.recompute_conf();
        // Review VCR-3: ack in the new epoch at once, so this member's acks never skip an epoch (an
        // ack's epoch rises by at most one, VC-5), however many decisions fall between two acks.
        if self.fixes_off & fix::VCR3 == 0 && !(self.quiet_until_member && !self.vcm.members.contains(&self.me)) {
            out.push(self.author_ack().entry);
        }
    }

    /// Make the version of removed member `x`'s chain that ends at its cut the one this replica
    /// holds: walk back from the cut id, fetch what is missing, replace any other version (8.2
    /// verdicts are re-derived along it), and keep exactly the cut (P2-4).
    pub(super) fn adopt_cut(&mut self, x: MemberId) {
        if self.vcm.cut_done.contains(&x) {
            return;
        }
        let (Some(&(cut, _)), Some(&cid)) = (self.vcm.evicted.get(&x), self.vcm.cut_ids.get(&x)) else { return };
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
                    self.vcm.ack_epoch_from.remove(&x);
                }
            }
            // Re-validating the cut's branch replays its acks, and each ack recomputes confirmation.
            // Confirming then, before that branch's entries act, would confirm frames whose
            // entries are about to change (found by fork9, review VC-2): settle first, confirm after.
            let fixed = self.fixes_off & fix::VC2 == 0;
            self.vcm.adopting = fixed;
            // Review VCR-6: reconciling before the branch is re-validated would apply its entries before
            // their horizon verdicts exist, a backdated one below the confirmed frame included. The
            // suspension, and the reconcile and recompute after, are enough.
            if fixed && self.fixes_off & fix::VCR6 != 0 {
                self.reconcile(x);
            }
            let mut out = Vec::new();
            self.extend_chain(x, &mut out);
            self.vcm.adopting = false;
        }
        self.reconcile(x);
        if self.contiguous.get(&x).copied().unwrap_or(0) >= cut {
            let v = self.ack_hist.get(&x).and_then(|a| a.range(..=cut).next_back()).map_or(0, |(_, h)| h + 1);
            self.vcm.evicted.insert(x, (cut, v));
            self.vcm.cut_done.insert(x);
            self.ev(format!("member {x} evicted (epoch {}): its chain is cut at seq {cut}; it no longer counts from frame {v}", self.vcm.epoch));
        }
        if self.fixes_off & fix::VC2 == 0 {
            self.recompute_conf();
        }
    }

    /// The certificate of the view change that started `epoch` (R3-8b).
    pub fn epoch_cert(&self, epoch: u32) -> Option<EpochCert> {
        let (c, round, sigs) = self.vcm.decided.get(epoch.checked_sub(1)? as usize)?;
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
        self.vcm.decided.iter().take_while(|d| d.0.boundary <= frame).count() as u32
    }

    /// A certificate for `fact`, a fact about `frame`: Attest entries of a strict majority of the
    /// members of the epoch active at that frame (P2-5's epoch-at-frame rule). Members that the next
    /// epoch excluded by proof do not count in that majority's size.
    pub fn cert_at(&self, fact: Fact, frame: u64) -> Option<(u32, Vec<(MemberId, Sig)>)> {
        let e = self.epoch_at(frame);
        let mut members: BTreeSet<MemberId> = self.members_at(e)?.into_iter().collect();
        if let Some((next, _, _)) = self.vcm.decided.get(e as usize) {
            for (m, _) in &next.excluded {
                members.remove(m);
            }
        }
        let mut c: BTreeMap<MemberId, Sig> = BTreeMap::new();
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
        if epoch > self.vcm.epoch {
            return None;
        }
        let mut m: BTreeSet<MemberId> = self.genesis.iter().copied().collect();
        for (c, _, _) in self.vcm.decided.iter().take(epoch as usize) {
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
    pub fn on_join(&mut self, j: MemberId, attests: Vec<Attest>, sig: Sig, expires: u64) {
        if self.is_static() || j == LOGIC {
            return;
        }
        let signed = self.fixes_off & fix::VC3R4 != 0 || (self.join_sig_ok(j, &sig, expires) && (self.fixes_off & fix::VC4R6 != 0 || expires > self.now));
        if !self.vcm.members.contains(&j) && !self.vcm.evicted.contains_key(&j) && j != self.me && signed {
            self.vcm.join_requests.insert(j);
            let r = self.join_reqs.entry(j).or_insert((sig, expires));
            if expires > r.1 {
                *r = (sig, expires);
            }
            if self.fixes_off & fix::VC3R4 == 0 {
                // Review VC3R-4: only the joiner can make its request, and every valid attestation in
                // any of its requests counts, so no request can hide another's (VCR-7's "the first
                // stands" let any peer pre-empt a joiner with an empty one).
                let have = self.vcm.join_attests.entry(j).or_default();
                for a in attests {
                    if attest_ok(&self.keys.public, &a, j, self.world_id) && !have.contains(&a) && have.len() < MAX_ATTESTS {
                        have.push(a);
                    }
                }
            } else if self.fixes_off & fix::VCR7 == 0 {
                // Review VCR-7 (round 3): the first request a joiner sends stands.
                self.vcm.join_attests.entry(j).or_insert(attests);
            } else {
                self.vcm.join_attests.insert(j, attests);
            }
            self.follow(j);
        }
    }

    /// Send `j` this replica's entries and store its own (a member about to join, in the spike).
    pub fn follow(&mut self, j: MemberId) {
        if !self.vcm.members.contains(&j) && j != self.me {
            self.known.insert(j);
            self.vcm.followers.insert(j);
        }
    }

    /// Who this replica sends its entries to: the members and the followers.
    pub fn audience(&self) -> Vec<MemberId> {
        self.vcm.members.iter().chain(self.vcm.followers.iter()).copied().filter(|m| *m != self.me).collect::<BTreeSet<_>>().into_iter().collect()
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

    /// Diagnostics: this replica's view-change state and how much of each chain it holds.
    pub fn vc_debug(&self) -> String {
        let pv: Vec<String> = self.vcm.vc.prevotes.iter().map(|(r, m)| format!("r{r}:{:?}", m.iter().map(|(k, v)| (*k, *v != 0)).collect::<Vec<_>>())).collect();
        let pc: Vec<String> = self.vcm.vc.precommits.iter().map(|(r, m)| format!("r{r}:{:?}", m.iter().map(|(k, v)| (*k, *v != 0)).collect::<Vec<_>>())).collect();
        format!(
            "now {} epoch {} round {} step {:?} since {:?} proposals {:?} prevotes [{}] precommits [{}] votes {:?} contiguous {:?} heard {:?}",
            self.now,
            self.vcm.epoch,
            self.vcm.vc.round,
            self.vcm.vc.step,
            self.vcm.vc.since,
            self.vcm.vc.proposals.keys().collect::<Vec<_>>(),
            pv.join(" "),
            pc.join(" "),
            self.vcm.votes.iter().map(|(t, v)| (*t, v.keys().copied().collect::<Vec<_>>())).collect::<Vec<_>>(),
            self.contiguous,
            self.heard
        )
    }

    /// The proposer of `round` of the view change ending `epoch`, if `epoch` is the current one.
    pub fn proposer_of(&self, epoch: u32, round: u32) -> Option<MemberId> {
        (epoch == self.vcm.epoch).then(|| self.proposer(epoch, round))
    }

    /// Answer `Msg::FetchEntry` (review VCR-8): the entry with that id, if held, and (review VC3R-7) up
    /// to `back` of its predecessors down its prev links, as far as this replica holds them.
    /// One run (option 2): the predecessors bottom up, the entry, and its author's entries above it
    /// up to its cover.
    pub fn on_fetch_entry(&self, id: EntryId, back: u8) -> Vec<Msg> {
        // Without runs on the wire (A/B): main's form, every entry signed on itself.
        if !self.run_wire {
            let mut out = Vec::new();
            let mut next = Some(id);
            while let Some(e) = next.and_then(|i| self.signed_entry(i)) {
                next = (out.len() < back.min(15) as usize && e.seq > 1).then_some(e.prev);
                out.push(Msg::Entry(e));
            }
            return out;
        }
        let Some(mut run) = self.run_of(id) else { return vec![] };
        let mut below = Vec::new();
        let mut e = run[0].entry.clone();
        while below.len() < back.min(15) as usize && e.seq > 1 {
            let Some(p) = self.entries.get(&e.prev) else { break };
            e = p.clone();
            below.push(SignedEntry { entry: e.clone(), sig: [0; 64] });
        }
        below.reverse();
        below.append(&mut run);
        if below.len() > WorldSim::RUN_MAX {
            return vec![];
        }
        if below.len() == 1 {
            return vec![Msg::Entry(below.pop().expect("one"))];
        }
        vec![Msg::Run { lean: false, heads: false, signed: true, entries: below }]
    }

    /// Answer `Msg::FetchValue`: the proposal entry carrying `value`, any version, with its cover.
    pub fn on_fetch_value(&self, value: u64) -> Vec<Msg> {
        self.vcm.proposal_ids.get(&value).and_then(|id| self.relay(*id)).map(|m| vec![m]).unwrap_or_default()
    }

    /// Ask `holder` for entry `id` (and `back` of its predecessors). Review VC4R-4: asked again with
    /// backoff while still missing (`take_fetches`); round 4 asked once, so one lost reply stopped a
    /// cut walk, an admin-entry fetch or a kick fetch for good.
    pub(super) fn want_entry(&mut self, holder: MemberId, id: EntryId, back: u8) {
        if holder == self.me {
            return;
        }
        if self.fixes_off & fix::VC4R4 == 0 || self.vcm.entry_asked.insert((holder, id)) {
            self.vcm.entry_wants.push((holder, id, back));
        }
    }

    /// [`Self::want_entry`] from every member this replica knows.
    pub(super) fn want_entry_all(&mut self, id: EntryId, back: u8) {
        let known: Vec<MemberId> = self.known.iter().copied().collect();
        for k in known {
            self.want_entry(k, id, back);
        }
    }
}

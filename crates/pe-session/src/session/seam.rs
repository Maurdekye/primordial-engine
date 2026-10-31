//! The membership seam (P2-3 S6b; @engine-architect's option (a)): the protocol core (log, sim,
//! R14, fetch) reads membership only through [`Membership`]. Two implementations:
//! * [`ViewChangeMembership`]: the view change (10.8, P2-4) and the D80 admin layer. Its fields are
//!   written by the view change (membership.rs, evict.rs), which is the only code that does.
//! * [`StaticMembership`]: a fixed member list, epoch 0 forever, every member an admin, no votes
//!   and no joins. M1 and pe-app's first node ([`WorldSim::with_static_membership`]).
//!
//! WorldSim holds the view change's state concretely (`vcm`) next to an optional static list, and
//! [`WorldSim::membership`] returns `&dyn Membership`: the view change's code then borrows its
//! state and the rest of the sim disjointly. A third implementation would need a
//! `Box<dyn Membership>` there instead.

use super::*;

/// What the protocol core needs to know about membership. Everything past `members` and `epoch`
/// is view-change state with a "nothing" default, which is the static answer.
pub trait Membership {
    /// The members of the current epoch.
    fn members(&self) -> &BTreeSet<MemberId>;
    fn epoch(&self) -> u32;
    /// The epoch this member states in its own acks (a test fixture may lie).
    fn ack_epoch(&self) -> u32 {
        self.epoch()
    }
    /// Removed members: (cut, from frame). Their entries up to the cut count.
    fn removed(&self) -> &BTreeMap<MemberId, (u32, u64)>;
    /// The id of a removed member's entry at its cut, once known.
    fn cut_id(&self, _m: MemberId) -> Option<EntryId> {
        None
    }
    /// The frame from which the session is closed (D80: no admin left).
    fn closed_at(&self) -> Option<u64> {
        None
    }
    /// This replica's frozen head of `m`'s chain, from its standing vote against `m`.
    fn frozen(&self, _m: MemberId) -> Option<u32> {
        None
    }
    /// The seq from which `m`'s chain is disputed by a proof of equivocation.
    fn proof_seq(&self, _m: MemberId) -> Option<u32> {
        None
    }
    /// The newest epoch `m`'s acks have stated (0 if none).
    fn last_ack_epoch(&self, _m: MemberId) -> u32 {
        0
    }
    /// The first seq of `m`'s acks from an epoch newer than mine: they count toward nothing.
    fn first_newer_ack(&self, _m: MemberId) -> Option<u32> {
        None
    }
    /// A joiner's first frame.
    fn admitted(&self, _m: MemberId) -> Option<u64> {
        None
    }
    /// After `m` precommitted a join: (its precommit's seq, the join's boundary).
    fn join_cap(&self, _m: MemberId) -> Option<(u32, u64)> {
        None
    }
    /// Confirmation is suspended while this replica switches to a removed member's cut branch.
    fn adopting(&self) -> bool {
        false
    }
    /// Whether `voter` has voted against `target` in this epoch: its frozen head is then its last
    /// word on `target`'s chain, and its later references never count toward stability (R3-1).
    fn voted_against(&self, _voter: MemberId, _target: MemberId) -> bool {
        false
    }
}

impl Membership for ViewChangeMembership {
    fn members(&self) -> &BTreeSet<MemberId> {
        &self.members
    }
    fn epoch(&self) -> u32 {
        self.epoch
    }
    fn ack_epoch(&self) -> u32 {
        self.fake_ack_epoch.unwrap_or(self.epoch)
    }
    fn removed(&self) -> &BTreeMap<MemberId, (u32, u64)> {
        &self.evicted
    }
    fn cut_id(&self, m: MemberId) -> Option<EntryId> {
        self.cut_ids.get(&m).copied()
    }
    fn closed_at(&self) -> Option<u64> {
        self.closed_at
    }
    fn frozen(&self, m: MemberId) -> Option<u32> {
        self.frozen.get(&m).copied()
    }
    fn proof_seq(&self, m: MemberId) -> Option<u32> {
        self.proofs.get(&m).map(|&(k, _, _)| k)
    }
    fn last_ack_epoch(&self, m: MemberId) -> u32 {
        self.ack_epoch_from.get(&m).and_then(|a| a.keys().next_back()).copied().unwrap_or(0)
    }
    fn first_newer_ack(&self, m: MemberId) -> Option<u32> {
        self.ack_epoch_from.get(&m).and_then(|a| a.range(self.epoch + 1..).map(|(_, s)| *s).min())
    }
    fn admitted(&self, m: MemberId) -> Option<u64> {
        self.admitted.get(&m).copied()
    }
    fn join_cap(&self, m: MemberId) -> Option<(u32, u64)> {
        self.join_caps.get(&m).copied()
    }
    fn adopting(&self) -> bool {
        self.adopting
    }
    fn voted_against(&self, voter: MemberId, target: MemberId) -> bool {
        self.votes.get(&target).is_some_and(|v| v.contains_key(&voter))
    }
}

/// The view change's bookkeeping during validation: written here, never read by the core.
impl ViewChangeMembership {
    /// `m`'s ack at `seq` stated `epoch`.
    pub(super) fn note_ack_epoch(&mut self, m: MemberId, epoch: u32, seq: u32) {
        self.ack_epoch_from.entry(m).or_default().entry(epoch).or_insert(seq);
    }
    /// The view-change fetch requests queued (by value and by id).
    pub(super) fn take_fetch_wants(&mut self) -> (Vec<(MemberId, u64)>, Vec<(MemberId, EntryId)>) {
        (std::mem::take(&mut self.value_wants), std::mem::take(&mut self.entry_wants))
    }
}

/// See the module documentation.
#[derive(Clone, Debug)]
pub struct StaticMembership {
    members: BTreeSet<MemberId>,
    none: BTreeMap<MemberId, (u32, u64)>,
}

impl StaticMembership {
    pub fn new(members: impl IntoIterator<Item = MemberId>) -> Self {
        StaticMembership { members: members.into_iter().collect(), none: BTreeMap::new() }
    }
}

impl Membership for StaticMembership {
    fn members(&self) -> &BTreeSet<MemberId> {
        &self.members
    }
    fn epoch(&self) -> u32 {
        0
    }
    fn removed(&self) -> &BTreeMap<MemberId, (u32, u64)> {
        &self.none
    }
}

impl WorldSim {
    /// The membership the protocol core reads.
    pub fn membership(&self) -> &dyn Membership {
        match &self.static_ms {
            Some(s) => s,
            None => &self.vcm,
        }
    }

    /// M1: a fixed member list for the session's life (see [`StaticMembership`]). The view change
    /// is off: no suspicion, no votes, no joins.
    pub fn with_static_membership(mut self) -> Self {
        self.static_ms = Some(StaticMembership::new(self.vcm.members.iter().copied()));
        self
    }

    pub fn is_static(&self) -> bool {
        self.static_ms.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With static membership a silent member is never voted out, a join is ignored, and the
    /// core reads the fixed list.
    #[test]
    fn static_membership_keeps_its_members() {
        let mut s = WorldSim::new(1, 1, &[1, 2, 3], SceneKind::Empty, SimConfig::default(), KeyRing::test(1)).with_static_membership();
        assert!(s.is_static());
        for f in 1..=1000 {
            s.now = f;
            s.advance(f);
            let _ = s.author_ack();
            assert!(s.suspects_due().is_empty() && s.view_change_due().is_empty());
        }
        s.on_join(9, Vec::new());
        assert_eq!(s.membership().members().iter().copied().collect::<Vec<_>>(), vec![1, 2, 3]);
        assert_eq!(s.membership().epoch(), 0);
        assert!(s.membership().removed().is_empty());
        // The same silent members are suspected under the view change.
        let mut v = WorldSim::new(1, 1, &[1, 2, 3], SceneKind::Empty, SimConfig::default(), KeyRing::test(1));
        let mut votes = 0;
        for f in 1..=1000 {
            v.now = f;
            v.advance(f);
            let _ = v.author_ack();
            votes += v.suspects_due().len();
        }
        assert!(votes > 0);
    }
}

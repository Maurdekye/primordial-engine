//! A member's graceful leave (world-model.md 2.4, 8; @coordinator-opus m003987d0d045 and mbdc0966ab9ab, after
//! @fable-reviewer m4598f19943d3): the world side of a final `Leave` entry, applied at its stamp on every replica, as
//! [`World::admit`] applies a join.

use std::sync::Arc;

use pe_core::{EntryRef, Frame, MemberKey, ObjectId};
use pe_seams::Version;

use crate::apply::Blocked;
use crate::components::Hand;
use crate::script::{AssetId, VoidKind, quorum};
use crate::world::{World, WorldError};

/// What [`World::leave`] did, each list in id (or asset) order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LeaveReport {
    /// The leaver's hands, removed outright: no tombstone, so no edit can undelete one.
    pub hands: Vec<ObjectId>,
    /// Objects whose claim by the leaver ended. Nothing else about them changes: an object keeps its velocity.
    pub released: Vec<ObjectId>,
    /// Children of a hand, moved to the hand's parent (a root, for M1's root hands), keeping their world pose and
    /// the hand's child order.
    pub moved: Vec<ObjectId>,
    /// Pending script commits voided at the leave (`QuorumUnreachable`): the leaver had not attested, and the
    /// attestations given plus the voters still members who could give one fall short of the quorum.
    pub voided: Vec<(AssetId, Version, VoidKind, Option<Blocked>)>,
}

impl World {
    /// `member`'s graceful leave at `frame`, from its final `Leave` entry `entry` (logic order: every replica calls it
    /// at the same frame). All or nothing; `NotAMember` if `member` holds no permission bits. In order:
    /// 1. each hand the member owns: its children move to the hand's parent, keeping world pose and order, then the
    ///    hand is removed outright, with its hand pack;
    /// 2. every claim by the member ends, as a release does (7.2: no cooldown);
    /// 3. the member's input rows go: its cooldowns and its push window, so a later readmission starts clean;
    /// 4. its permission bits are dropped;
    /// 5. a pending commit whose quorum the leave makes unreachable is voided now (logic.md 6.2a). The voters stay
    ///    those at `c`, and an attestation the leaver gave before leaving still counts. The leaver's own pending
    ///    commit is left to 6.2a: it voids at activation as `AuthorRevokedAtActivation`.
    ///
    /// Everything else the member made or set stays: its objects, the protections it set (an admin can lift them).
    pub fn leave(&mut self, member: MemberKey, entry: EntryRef, frame: Frame) -> Result<LeaveReport, WorldError> {
        if !self.members.contains_key(&member) {
            return Err(WorldError::NotAMember(member));
        }
        let v = Version { entry, op: 0 };
        let mut w = self.clone();
        let mut report = LeaveReport::default();
        let hands: Vec<ObjectId> = w
            .store
            .objects()
            .iter()
            .copied()
            .filter(|&id| w.store.get::<Hand>(id).is_some_and(|h| h.owner == member))
            .collect();
        for &h in &hands {
            let parent = w.store.parent(h);
            let at = parent.and_then(|p| w.store.children(p).position(|c| c == h));
            let kids: Vec<ObjectId> = w.store.children(h).collect();
            for (k, &c) in kids.iter().enumerate() {
                w.store.set_parent_keep_world(c, parent, at.map(|i| i + k))?;
                w.touch(c, v, true);
                report.moved.push(c);
            }
            if let Some(p) = parent {
                w.touch(p, v, true);
            }
            w.store.destroy(h)?;
            Arc::make_mut(&mut w.inputs).packs.remove(&h);
        }
        report.moved.sort();
        report.hands = hands;
        report.released = w.claims.iter().filter(|(_, c)| c.member == member).map(|(&o, _)| o).collect();
        if !report.released.is_empty() || w.claims.keys().any(|o| report.hands.contains(o)) {
            Arc::make_mut(&mut w.claims).retain(|o, c| c.member != member && !report.hands.contains(o));
        }
        let t = Arc::make_mut(&mut w.inputs);
        t.cooldowns.retain(|(m, _), _| *m != member);
        t.pushes.remove(&member);
        w.set_member(member, 0);
        let due: Vec<AssetId> = w
            .pending
            .iter()
            .filter(|(_, p)| {
                let attested = |k: &MemberKey| p.attested.iter().any(|(a, _)| a == k);
                let could = p.voters.iter().filter(|k| !attested(k) && w.members.contains_key(k)).count();
                p.void.is_none()
                    && p.voters.contains(&member)
                    && !attested(&member)
                    && p.attested.len() + could < quorum(p.voters.len())
            })
            .map(|(a, _)| *a)
            .collect();
        for a in due {
            let p = Arc::make_mut(&mut w.pending).get_mut(&a).expect("present");
            p.void = Some((frame, VoidKind::QuorumUnreachable));
            report.voided.push((a, p.version, VoidKind::QuorumUnreachable, None));
        }
        *self = w;
        Ok(report)
    }
}

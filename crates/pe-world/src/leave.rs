//! A member's graceful leave (world-model.md 2.4, 8; @coordinator-opus, after
//! @fable-reviewer): the world side of a final `Leave` entry, applied at its stamp on every replica, as
//! [`World::admit`] applies a join.

use std::sync::Arc;

use pe_core::{EntryRef, MemberKey, ObjectId};
use pe_seams::Version;

use crate::components::{Hand, ids};
use crate::world::{Tombstone, World, WorldError};

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
}

impl World {
    /// `member`'s graceful leave, from its final `Leave` entry `entry`, applied at the entry's stamp (every replica
    /// at the same frame, in agreed order). All or nothing. Its only error is `NotAMember`, for a key that holds no
    /// permission bits; every step below is infallible, so pe-replica treats any other `Err` as a bug. In order:
    /// 1. each hand the member owns: its children move to the hand's parent, keeping world pose and order, then the
    ///    hand is removed outright, with its hand pack; a deleted hand of the member's leaves no tombstone behind;
    /// 2. every claim by the member ends, as a release does (7.2: no cooldown);
    /// 3. the member's input rows go: its cooldowns and its push window, so a later readmission starts clean;
    /// 4. its permission bits are dropped;
    /// 5. pending script commits are left to 6.2a: the voters stay those at `c` (an attestation the leaver gave
    ///    still counts), and one whose quorum the leave makes unreachable is voided at this frame's end by the shared
    ///    void rule (`QuorumUnreachable`); the leaver's own voids as `AuthorRevokedAtActivation` if it reaches its
    ///    quorum, else as `QuorumUnreachable` or, at `D`, `Unattested` (world-model.md 2.4).
    ///
    /// Everything else the member made or set stays: its objects, and the protections it set (nobody can lift those
    /// until agent powers lets an admin clear a lock, world-model.md 2.4).
    pub fn leave(&mut self, member: MemberKey, entry: EntryRef) -> Result<LeaveReport, WorldError> {
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
                // A final Leave must apply on every replica, so the move can't fail: if keeping the world pose under
                // the hand's parent is refused (a rewritten transform past the limits), the child becomes a root
                // keeping its world pose, and failing that a root keeping its local transform, which needs no
                // rewrite (@chunks-reviewer's L1).
                let up = w.store.set_parent_keep_world(c, parent, at.map(|i| i + k));
                let moved = up
                    .or_else(|_| w.store.set_parent_keep_world(c, None, None))
                    .or_else(|_| w.store.set_parent(c, None, None));
                debug_assert!(moved.is_ok(), "a hand's child always moves up: {moved:?}");
                w.touch(c, v, true);
                report.moved.push(c);
            }
            if let Some(p) = parent {
                w.touch(p, v, true);
            }
            let gone = w.store.destroy(h);
            debug_assert!(gone.is_ok(), "the hand is live: {gone:?}");
            Arc::make_mut(&mut w.inputs).packs.remove(&h);
        }
        // A hand of the member's that an edit deleted lies in a tombstone: drop it, so no `Undelete` brings back a
        // hand owned by a non-member (@chunks-reviewer's L2).
        let ghost = |t: &Tombstone| {
            t.rows.iter().any(|(c, bytes, _)| {
                *c == ids::HAND
                    && bytes.len() == size_of::<Hand>()
                    && bytemuck::pod_read_unaligned::<Hand>(bytes).owner == member
            })
        };
        if w.tombstones.values().any(ghost) {
            Arc::make_mut(&mut w.tombstones).retain(|_, t| !ghost(t));
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
        *self = w;
        Ok(report)
    }
}

//! A member's graceful leave (world-model.md 2.4, 8; @coordinator-opus, after
//! @fable-reviewer): the world side of a final `Leave` entry, applied at its stamp on every replica, as
//! [`World::admit`] applies a join.

use std::sync::Arc;

use pe_core::{EntryRef, MemberKey, ObjectId};
use pe_seams::Version;

use crate::components::{Hand, Player, ids};
use crate::world::{Tombstone, World, WorldError};

/// What [`World::leave`] did, each list in id (or asset) order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LeaveReport {
    /// The leaver's hands, removed outright: no tombstone, so no edit can undelete one.
    pub hands: Vec<ObjectId>,
    /// Objects whose claim by the leaver ended. Nothing else about them changes: an object keeps its velocity.
    pub released: Vec<ObjectId>,
    /// Children of a hand, moved to the hand's parent (a root, for M1's root hands), keeping their world pose and
    /// the hand's child order; and children of the leaver's player body, moved to the roots.
    pub moved: Vec<ObjectId>,
    /// The leaver's player bodies (8.2), removed outright after its hands, with their Move packs: each body and then
    /// the rest of its subtree (its drawn shape), as `SimStore::subtree` lists them.
    pub bodies: Vec<ObjectId>,
}

impl World {
    /// `member`'s graceful leave, from its final `Leave` entry `entry`, applied at the entry's stamp (every replica
    /// at the same frame, in agreed order). All or nothing. Its only error is `NotAMember`, for a key that holds no
    /// permission bits; every step below is infallible, so pe-replica treats any other `Err` as a bug. In order:
    /// 1. each hand the member owns: its children move to the hand's parent, keeping world pose and order, then the
    ///    hand is removed outright, with its hand pack; a deleted hand of the member's leaves no tombstone behind;
    ///    then its player body, if the preset gave it one (8.2): its children become roots keeping their world pose,
    ///    and the body is removed outright with its Move pack;
    /// 2. every claim by the member ends, as a release does (7.2: no cooldown);
    /// 3. the member's input rows go: its cooldowns, its push window and its published per-player values (7.4), so a
    ///    later readmission starts clean;
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
            // A body's hand's children go past the body, which goes below: they become roots (8.2).
            let parent = w.store.parent(h).filter(|&p| w.store.get_raw(ids::PLAYER, p).is_none());
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
            let t = Arc::make_mut(&mut w.inputs);
            t.packs.remove(&h);
            t.body_packs.remove(&h);
        }
        // Then its player body (8.2), with its whole subtree: what is left under it is logic's (its drawn shape),
        // since no edit puts anything directly under a body, and its hands' children became roots above. It goes
        // with its Move pack.
        let bodies: Vec<ObjectId> = if w.store.registry().get(ids::PLAYER).is_some() {
            w.store.iter::<Player>().filter(|(_, p)| p.owner == member).map(|(b, _)| b).collect()
        } else {
            Vec::new()
        };
        for &b in &bodies {
            report.bodies.extend(w.store.subtree(b));
            let gone = w.store.destroy(b);
            debug_assert!(gone.is_ok(), "the body is live: {gone:?}");
            Arc::make_mut(&mut w.inputs).moves.remove(&b);
        }
        // The removed objects' per-player rows go with them: these objects leave no tombstone, so nothing else would
        // drop them (logic.md 7.4; @world-model's M1). A body's subtree can hold script components with per-player
        // fields.
        let gone = |o: &ObjectId| hands.contains(o) || report.bodies.contains(o);
        if w.per_player.modes.keys().any(|k| k.object.is_some_and(|o| gone(&o)))
            || w.per_player.feed.keys().any(|k| gone(&k.object))
        {
            let t = Arc::make_mut(&mut w.per_player);
            for o in hands.iter().chain(&report.bodies) {
                t.drop_object(*o);
            }
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
        // Its published values go too (logic.md 7.4; @world-model): gathers count only members, and
        // the table would otherwise grow with every member who ever joined.
        if w.per_player.feed.keys().any(|k| k.member == member) {
            Arc::make_mut(&mut w.per_player).drop_member(member);
        }
        w.set_member(member, 0);
        *self = w;
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use pe_core::{EntryRef, Frame, Hash, MemberKey, ObjectId, Pos};
    use pe_sim::ComponentId;

    use crate::per_player::{FeedKey, FeedValue, Mode, ModeKey, ModeRow, Selector};
    use crate::preset::{m1_line, plane_on};
    use crate::{BlobStore, perm};

    /// A leave removes its hands and its body's subtree without tombstones, so their per-player rows go with them,
    /// in both tables, other members' published values included; rows on other objects stay (@world-model's M1).
    #[test]
    fn a_leave_drops_the_per_player_rows_of_the_objects_it_removes() {
        let (ka, kb) = (MemberKey([1; 32]), MemberKey([2; 32]));
        let line = m1_line();
        let mut w = plane_on(line, 1, BlobStore::default(), Pos::ORIGIN);
        w.admit(line, ka, perm::ADMIN | perm::EDIT, Frame(0), Pos::ORIGIN).unwrap();
        let hands_b = w.admit(line, kb, perm::EDIT, Frame(0), Pos::ORIGIN).unwrap();
        // B's body's drawn shape, a child of its body, and one of B's hands; and A's body, which stays.
        let shape_b = ObjectId::body(line, kb, 1, Frame(0));
        let body_a = ObjectId::body(line, ka, 0, Frame(0));
        assert!(w.store.exists(shape_b) && w.store.exists(body_a));
        let c = ComponentId(2000);
        let row = ModeRow { mode: Mode::Decoupled, selector: Selector::Latest };
        let t = Arc::make_mut(&mut w.per_player);
        for o in [shape_b, hands_b[0], body_a] {
            t.modes.insert(ModeKey { component: c, field: 0, object: Some(o) }, row);
            t.feed.insert(
                FeedKey { object: o, component: c, field: 0, member: ka },
                FeedValue { value: vec![7; 4], frame: Frame(1), index: 0 },
            );
        }
        t.modes.insert(ModeKey { component: c, field: 0, object: None }, row);
        let r = w.leave(kb, EntryRef(Hash([0x1e; 32]))).unwrap();
        assert!(r.bodies.contains(&shape_b), "{r:?}");
        let objects: Vec<_> = w.per_player.modes.keys().map(|k| k.object).collect();
        assert_eq!(objects, vec![None, Some(body_a)]);
        let fed: Vec<_> = w.per_player.feed.keys().map(|k| k.object).collect();
        assert_eq!(fed, vec![body_a]);
    }
}

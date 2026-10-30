//! Identifiers. All are plain integers with a total order, so containers keyed by them iterate
//! identically on every peer (design 4.3 rule 4).

use bytemuck::{Pod, Zeroable};

/// A session member. Member 0 is world logic ([`LOGIC`]): its actions are computed identically on every
/// peer and never networked.
pub type MemberId = u32;

/// The world-logic member.
pub const LOGIC: MemberId = 0;

/// A sim frame number: frame `f` is the state *before* frame `f`'s entries are applied (design 6.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Frame(pub u64);

impl Frame {
    pub const ZERO: Frame = Frame(0);

    /// The arithmetic here is checked explicitly, so overflow panics identically in debug and release
    /// builds (review P21-8). Frames come nowhere near 2^64; state transfers refuse frames above 2^62.
    pub fn next(self) -> Frame {
        Frame(self.0.checked_add(1).expect("frame overflow"))
    }

    /// Frames from `earlier` to `self`; `None` if `earlier` is later.
    pub fn since(self, earlier: Frame) -> Option<u64> {
        self.0.checked_sub(earlier.0)
    }
}

impl core::ops::Add<u64> for Frame {
    type Output = Frame;
    fn add(self, n: u64) -> Frame {
        Frame(self.0.checked_add(n).expect("frame overflow"))
    }
}

impl core::ops::Sub<u64> for Frame {
    type Output = Frame;
    fn sub(self, n: u64) -> Frame {
        Frame(self.0.checked_sub(n).expect("frame underflow"))
    }
}

/// A sim object in one world: `(creator, counter)`, assigned in the sim (design 16.1). Ordered by creator,
/// then counter. Counters start at 1, so [`ObjectId::NONE`] (0, 0) is never a live object.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Pod, Zeroable)]
#[repr(C)]
pub struct ObjectId {
    pub creator: MemberId,
    pub counter: u32,
}

impl ObjectId {
    pub const NONE: ObjectId = ObjectId { creator: 0, counter: 0 };

    pub const fn new(creator: MemberId, counter: u32) -> Self {
        ObjectId { creator, counter }
    }

    pub fn is_none(self) -> bool {
        self == ObjectId::NONE
    }

    /// `Some(self)` unless this is [`ObjectId::NONE`].
    pub fn some(self) -> Option<ObjectId> {
        (!self.is_none()).then_some(self)
    }

    /// The id as one integer, preserving the order.
    pub fn to_u64(self) -> u64 {
        ((self.creator as u64) << 32) | self.counter as u64
    }

    pub fn from_u64(v: u64) -> Self {
        ObjectId { creator: (v >> 32) as u32, counter: v as u32 }
    }
}

/// A world (R14). Global object ids name their birth world (P2-5).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Pod, Zeroable)]
#[repr(transparent)]
pub struct WorldId(pub u32);

/// An object's identity across worlds (R14): the world it was born in and its id there. It never changes
/// when the object moves between worlds; the object gets a new local [`ObjectId`] in each world.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Pod, Zeroable)]
#[repr(C)]
pub struct GlobalId {
    pub birth_world: WorldId,
    pub object: ObjectId,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_id_order_matches_u64() {
        let ids = [ObjectId::new(0, 5), ObjectId::new(1, 1), ObjectId::new(1, 2), ObjectId::new(7, 0)];
        for w in ids.windows(2) {
            assert!(w[0] < w[1]);
            assert!(w[0].to_u64() < w[1].to_u64());
            assert_eq!(ObjectId::from_u64(w[0].to_u64()), w[0]);
        }
        assert_eq!(ObjectId::NONE.some(), None);
        assert_eq!(core::mem::size_of::<ObjectId>(), 8);
        assert_eq!(core::mem::size_of::<GlobalId>(), 12);
    }
}

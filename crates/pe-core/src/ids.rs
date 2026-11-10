//! Identifiers (world-model.md section 2). All have a total order, so containers keyed by them iterate identically on
//! every machine (design 4.3 rule 4).
//!
//! * [`ObjectId`]: 128 bits, world-scoped, **derived, never counted** (2.1), so an object has the same id on every
//!   machine, before and after a rollback, and in a merge, whatever the arrival order.
//! * [`LineId`]: a world is a line in one tree of versions rooted at the fixed, empty origin (2.3).
//! * [`MemberKey`]: a member is named by its 32-byte public key wherever the name is stored in the world (2.2).
//! * [`ChainId`], [`EntryRef`]: an author's chain on one line, and one entry.

use bytemuck::{Pod, Zeroable};

use crate::commit::{Hash, Hasher};

/// A sim frame number: frame `f` is the state *before* frame `f`'s entries are applied (design 6.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Frame(pub u64);

impl Frame {
    pub const ZERO: Frame = Frame(0);

    /// The arithmetic here is checked explicitly, so overflow panics identically in debug and release
    /// builds (review P21-8). Frames come nowhere near 2^64.
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

/// An object of one world (2.1). Ordered as a `u128`; the order means nothing beyond being deterministic.
/// [`ObjectId::NONE`] (0) never names an object.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId(pub u128);

impl ObjectId {
    pub const NONE: ObjectId = ObjectId(0);

    pub fn is_none(self) -> bool {
        self == ObjectId::NONE
    }

    /// `Some(self)` unless this is [`ObjectId::NONE`].
    pub fn some(self) -> Option<ObjectId> {
        (!self.is_none()).then_some(self)
    }

    /// The first 16 bytes of a hash, as a little-endian `u128`; 0 becomes 1, so a derived id is never NONE.
    fn from_hash(h: Hash) -> ObjectId {
        let v = u128::from_le_bytes(h.0[..16].try_into().unwrap());
        ObjectId(if v == 0 { 1 } else { v })
    }

    /// The `k`-th object an edit entry creates (2.1): `BLAKE3("pe.object" ‖ line ‖ author ‖ chain ‖ seq ‖ k)[..16]`.
    pub fn created(line: LineId, author: MemberKey, chain: ChainId, seq: u64, k: u32) -> ObjectId {
        let mut h = Hasher::new("pe.object");
        h.write_hash(&line.0).write(&author.0).write_hash(&chain.0).write_u64(seq).write_u32(k);
        ObjectId::from_hash(h.finish())
    }

    /// The `n`-th applied spawn of `spawner` in `frame`, by world logic (2.1).
    pub fn spawned(line: LineId, spawner: ObjectId, frame: Frame, n: u32) -> ObjectId {
        let mut h = Hasher::new("pe.spawn");
        h.write_hash(&line.0).write_u128(spawner.0).write_u64(frame.0).write_u32(n);
        ObjectId::from_hash(h.finish())
    }

    /// A member's hand, created by world logic at admission (8): `side` 0 is left, 1 is right.
    pub fn hand(line: LineId, member: MemberKey, side: u8, admission: Frame) -> ObjectId {
        let mut h = Hasher::new("pe.hand");
        h.write_hash(&line.0).write(&member.0).write_u8(side).write_u64(admission.0);
        ObjectId::from_hash(h.finish())
    }
}

/// An [`ObjectId`] as stored inside a component row: two little-endian `u64` words, so rows stay 8-byte aligned
/// (a `u128` is 16-byte aligned on x86-64; world-model.md 3.2, `ObjectRef`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct ObjectRef {
    pub lo: u64,
    pub hi: u64,
}

impl ObjectRef {
    pub const NONE: ObjectRef = ObjectRef { lo: 0, hi: 0 };

    pub fn id(self) -> ObjectId {
        ObjectId(((self.hi as u128) << 64) | self.lo as u128)
    }

    pub fn is_none(self) -> bool {
        self == ObjectRef::NONE
    }
}

impl From<ObjectId> for ObjectRef {
    fn from(id: ObjectId) -> ObjectRef {
        ObjectRef { lo: id.0 as u64, hi: (id.0 >> 64) as u64 }
    }
}

impl From<ObjectRef> for ObjectId {
    fn from(r: ObjectRef) -> ObjectId {
        r.id()
    }
}

/// A member's 32-byte public key (2.2). The all-zero key is world logic ([`MemberKey::LOGIC`]) and is never a
/// member's key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Pod, Zeroable)]
#[repr(transparent)]
pub struct MemberKey(pub [u8; 32]);

impl MemberKey {
    /// World logic: the author of logic-caused changes.
    pub const LOGIC: MemberKey = MemberKey([0; 32]);

    pub fn is_logic(&self) -> bool {
        *self == MemberKey::LOGIC
    }
}

/// A line: one world in the tree of versions (2.3). Its id is the hash of the entry that created it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LineId(pub Hash);

/// The origin's record (2.3): one entry, fixed for ever. Changing these bytes would leave worlds made before and
/// after with no common ancestor; a test pins them and their hash.
pub const ORIGIN_ENTRY: &[u8] = b"pe.origin.v1";

impl LineId {
    /// The origin: the empty world every line descends from. `BLAKE3(ORIGIN_ENTRY)`, plain.
    pub fn origin() -> LineId {
        LineId(Hash::plain(ORIGIN_ENTRY))
    }
}

/// An author's chain on one line (2.1): `BLAKE3("pe.chain" ‖ line ‖ key ‖ nonce)`, the nonce being 32 random bytes
/// in the chain's first entry. (chain id, seq) is unique for the world's life.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChainId(pub Hash);

impl ChainId {
    pub fn new(line: LineId, key: MemberKey, nonce: [u8; 32]) -> ChainId {
        let mut h = Hasher::new("pe.chain");
        h.write_hash(&line.0).write(&key.0).write(&nonce);
        ChainId(h.finish())
    }
}

/// An entry, by pe-log's 256-bit entry hash (2.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntryRef(pub Hash);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_ref_round_trip_and_order() {
        let ids = [ObjectId(1), ObjectId(u64::MAX as u128), ObjectId((u64::MAX as u128) + 1), ObjectId(u128::MAX)];
        for w in ids.windows(2) {
            assert!(w[0] < w[1]);
        }
        for id in ids {
            assert_eq!(ObjectRef::from(id).id(), id);
        }
        assert_eq!(core::mem::size_of::<ObjectRef>(), 16);
        assert_eq!(core::mem::align_of::<ObjectRef>(), 8);
        assert_eq!(ObjectId::NONE.some(), None);
    }

    /// The origin is fixed for ever (2.3). If this test fails, the change is wrong, not the test.
    #[test]
    fn origin_is_pinned() {
        assert_eq!(ORIGIN_ENTRY, b"pe.origin.v1");
        assert_eq!(LineId::origin().0, Hash::plain(b"pe.origin.v1"));
        assert_eq!(
            LineId::origin().0.to_string(),
            "9a570a9c46078da1b28a58daddf02dcea9c8bb31f4efc8ab61089b552b1ddb9e",
            "pinned origin hash"
        );
    }

    #[test]
    fn derivations_differ_by_every_input() {
        let line = LineId::origin();
        let other = LineId(Hash::plain(b"another line"));
        let key = MemberKey([7; 32]);
        let chain = ChainId::new(line, key, [1; 32]);
        let a = ObjectId::created(line, key, chain, 5, 0);
        assert_ne!(a, ObjectId::created(other, key, chain, 5, 0));
        assert_ne!(a, ObjectId::created(line, MemberKey([8; 32]), chain, 5, 0));
        assert_ne!(a, ObjectId::created(line, key, ChainId::new(line, key, [2; 32]), 5, 0));
        assert_ne!(a, ObjectId::created(line, key, chain, 6, 0));
        assert_ne!(a, ObjectId::created(line, key, chain, 5, 1));
        assert_eq!(a, ObjectId::created(line, key, chain, 5, 0));
        assert_ne!(ObjectId::spawned(line, a, Frame(3), 0), ObjectId::spawned(line, a, Frame(3), 1));
        assert_ne!(ObjectId::hand(line, key, 0, Frame(9)), ObjectId::hand(line, key, 1, Frame(9)));
        assert_ne!(ObjectId::hand(line, key, 0, Frame(9)), ObjectId::hand(line, key, 0, Frame(10)));
        assert!(!a.is_none());
    }
}

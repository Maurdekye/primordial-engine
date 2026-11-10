//! The id, hash and position types every seam uses.
//!
//! **Transition (v0).** These belong in pe-core (world-model.md 9.1; accepted for S1). WS-world's pe-core change,
//! which defines them there, is on `world/core-v1` (stage A at 426d8420) and lands with pe-sim and pe-physics
//! adapted. Until then pe-seams carries copies of their shapes, taken from 426d8420: the same names, fields, derives
//! and derivations, so code written against `pe_seams::{Hash, ObjectId, ...}` compiles unchanged after the switch.
//! When `world/core-v1` lands, the same landing replaces this module's definitions with
//! `pub use pe_core::{...}` (@world-model makes that edit; @engine-architect reviews it). [`Frame`] is already
//! pe-core's and is re-exported.
//!
//! [`Pos`] here is the type wall's shape only: no reader, no constructor from numbers, `Pos - Pos` total, and
//! moving by [`Pos::checked_add`]. Its formulas are 426d8420's. Its encoding, cells and rows stay in pe-core.

#![allow(clippy::needless_range_loop)] // per-axis loops read clearest with an index, as in pe-core

use core::fmt;

pub use pe_core::Frame;

// ---- hashes --------------------------------------------------------------------------------------------------------

/// A 256-bit BLAKE3 hash: the only hash that may appear in an entry id, a cut, a vote, a root or an id
/// (world-model.md 6.1). XXH64 stays in pe-core for non-commitment uses only.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Hash(pub [u8; 32]);

impl Hash {
    pub const ZERO: Hash = Hash([0; 32]);

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The hash of `bytes` under the context `context` (BLAKE3's key-derivation mode), so a hash made for one
    /// purpose never equals one made for another.
    pub fn of(context: &str, bytes: &[u8]) -> Hash {
        let mut h = Hasher::new(context);
        h.write(bytes);
        h.finish()
    }

    /// Plain BLAKE3 with no context: only for content addressing (a blob's name) and the origin entry.
    pub fn plain(bytes: &[u8]) -> Hash {
        Hash(*blake3::hash(bytes).as_bytes())
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in &self.0[..8] {
            write!(f, "{b:02x}")?;
        }
        write!(f, "…")
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in &self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// A domain-separated streaming BLAKE3 hasher. Every field fed in is fixed-length or length-prefixed by the caller.
#[derive(Clone)]
pub struct Hasher(blake3::Hasher);

impl Hasher {
    pub fn new(context: &str) -> Self {
        Hasher(blake3::Hasher::new_derive_key(context))
    }

    pub fn write(&mut self, bytes: &[u8]) -> &mut Self {
        self.0.update(bytes);
        self
    }

    pub fn write_u8(&mut self, v: u8) -> &mut Self {
        self.write(&[v])
    }

    pub fn write_u32(&mut self, v: u32) -> &mut Self {
        self.write(&v.to_le_bytes())
    }

    pub fn write_u64(&mut self, v: u64) -> &mut Self {
        self.write(&v.to_le_bytes())
    }

    pub fn write_u128(&mut self, v: u128) -> &mut Self {
        self.write(&v.to_le_bytes())
    }

    pub fn write_hash(&mut self, h: &Hash) -> &mut Self {
        self.write(&h.0)
    }

    /// A u64 length, then the bytes.
    pub fn write_len_prefixed(&mut self, bytes: &[u8]) -> &mut Self {
        self.write_u64(bytes.len() as u64);
        self.write(bytes)
    }

    pub fn finish(&self) -> Hash {
        Hash(*self.0.finalize().as_bytes())
    }
}

// ---- ids -----------------------------------------------------------------------------------------------------------

/// An object in a world: 128 bits, derived, never allocated (world-model.md 2.1). Its derivations
/// (`created`, `spawned`, `hand`) are pe-core's and arrive with it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId(pub u128);

impl ObjectId {
    pub const NONE: ObjectId = ObjectId(0);

    /// As pe-core's `ObjectId::from_u128`, which world/core-v1 brings: code that uses these two compiles before and
    /// after the switch to pe-core's type, whose fields are private.
    pub const fn from_u128(v: u128) -> ObjectId {
        ObjectId(v)
    }

    /// As pe-core's `ObjectId::to_u128`.
    pub const fn to_u128(self) -> u128 {
        self.0
    }

    pub fn is_none(self) -> bool {
        self == ObjectId::NONE
    }

    /// `Some(self)` unless this is [`ObjectId::NONE`].
    pub fn some(self) -> Option<ObjectId> {
        (!self.is_none()).then_some(self)
    }

    /// The first 16 bytes of a hash, as a little-endian `u128`; 0 becomes 1, so a derived id is never NONE.
    fn from_hash(h: Hash) -> ObjectId {
        let mut b = [0u8; 16];
        b.copy_from_slice(&h.0[..16]);
        let v = u128::from_le_bytes(b);
        ObjectId(if v == 0 { 1 } else { v })
    }

    /// The `k`-th object an edit entry creates (world-model.md 2.1):
    /// `BLAKE3("pe.object" ‖ line ‖ author ‖ chain ‖ seq ‖ k)[..16]`.
    pub fn created(line: LineId, author: MemberKey, chain: ChainId, seq: u64, k: u32) -> ObjectId {
        let mut h = Hasher::new("pe.object");
        h.write_hash(&line.0).write(&author.0).write_hash(&chain.0).write_u64(seq).write_u32(k);
        ObjectId::from_hash(h.finish())
    }

    /// The `n`-th applied spawn of `spawner` in `frame`, by world logic (world-model.md 2.1).
    pub fn spawned(line: LineId, spawner: ObjectId, frame: Frame, n: u32) -> ObjectId {
        let mut h = Hasher::new("pe.spawn");
        h.write_hash(&line.0).write_u128(spawner.0).write_u64(frame.0).write_u32(n);
        ObjectId::from_hash(h.finish())
    }

    /// A member's hand, created by world logic at admission (world-model.md 8): `side` 0 is left, 1 is right.
    pub fn hand(line: LineId, member: MemberKey, side: u8, admission: Frame) -> ObjectId {
        let mut h = Hasher::new("pe.hand");
        h.write_hash(&line.0).write(&member.0).write_u8(side).write_u64(admission.0);
        ObjectId::from_hash(h.finish())
    }
}

/// A member's 32-byte Ed25519 public key (world-model.md 2.2). The all-zero key is world logic
/// ([`MemberKey::LOGIC`]) and is never a member's key. It is also the peer's identity on the network
/// (transport.md 2.2; S4's [`crate::PeerId`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MemberKey(pub [u8; 32]);

impl MemberKey {
    /// World logic: the author of logic-caused changes.
    pub const LOGIC: MemberKey = MemberKey([0; 32]);

    pub fn is_logic(&self) -> bool {
        *self == MemberKey::LOGIC
    }
}

/// A line: one world in the tree of versions (world-model.md 2.3). Its id is the hash of the entry that created it.
/// Entries on a line are signed over its id (S1), and cuts carry it (S3). There is no other world id.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LineId(pub Hash);

/// The origin's record (world-model.md 2.3): one entry, fixed for ever, byte-identical across engine versions.
pub const ORIGIN_ENTRY: &[u8] = b"pe.origin.v1";

impl LineId {
    /// The origin: the empty world every line descends from. `BLAKE3(ORIGIN_ENTRY)`, plain.
    pub fn origin() -> LineId {
        LineId(Hash::plain(ORIGIN_ENTRY))
    }
}

/// An author's chain on one line (world-model.md 2.1): `BLAKE3("pe.chain" ‖ line ‖ key ‖ nonce)`, the nonce being
/// 32 random bytes in the chain's first entry. (chain id, seq) is unique for the world's life; a member whose chain
/// starts again takes a new nonce, so a restart is a second chain, never an equivocation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChainId(pub Hash);

impl ChainId {
    pub fn new(line: LineId, key: MemberKey, nonce: [u8; 32]) -> ChainId {
        let mut h = Hasher::new("pe.chain");
        h.write_hash(&line.0).write(&key.0).write(&nonce);
        ChainId(h.finish())
    }
}

/// An entry's id: the hash of its signed bytes (S1, [`crate::Entry::id`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntryRef(pub Hash);

// ---- positions -----------------------------------------------------------------------------------------------------

const CHUNK: f64 = 1024.0;
const RESERVED_CHUNK: i64 = i64::MIN;

/// A position left the supported range, or was not finite (world-model.md 4.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutOfRange;

impl fmt::Display for OutOfRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "position out of range")
    }
}

/// A displacement in metres, along the world axes. Ordinary floating point.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Offset(pub [f64; 3]);

impl Offset {
    pub const ZERO: Offset = Offset([0.0; 3]);

    pub fn length2(&self) -> f64 {
        let d = self.0;
        d[0] * d[0] + d[1] * d[1] + d[2] * d[2]
    }

    pub fn length(&self) -> f64 {
        self.length2().sqrt()
    }
}

impl core::ops::Neg for Offset {
    type Output = Offset;
    fn neg(self) -> Offset {
        Offset([-self.0[0], -self.0[1], -self.0[2]])
    }
}

/// A place in the world, exact everywhere, fields private (world-model.md 4.2). The only way to plain numbers is
/// subtracting another `Pos`; a renderer subtracts the camera's, so large coordinates never reach a GPU float.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pos {
    chunk: [i64; 3],
    local: [f64; 3],
}

fn canon(x: f64) -> f64 {
    if x == 0.0 { 0.0 } else { x }
}

impl Pos {
    /// The world origin constant: chunk 0, offset 0.
    pub const ORIGIN: Pos = Pos { chunk: [0; 3], local: [0.0; 3] };

    /// The minimum corner of a level-0 chunk (1,024 m).
    pub fn at_chunk(chunk: [i64; 3]) -> Result<Pos, OutOfRange> {
        if chunk.contains(&RESERVED_CHUNK) {
            return Err(OutOfRange);
        }
        Ok(Pos { chunk, local: [0.0; 3] })
    }

    /// `self` moved by `d`: exact in the chunk part; each axis's offset is rounded once, then carried into whole
    /// chunks exactly. A non-finite `d`, or a result past the range, is [`OutOfRange`]. There is no `Pos + Offset`.
    pub fn checked_add(self, d: Offset) -> Result<Pos, OutOfRange> {
        let mut out = self;
        for a in 0..3 {
            let v = self.local[a] + d.0[a];
            if !v.is_finite() {
                return Err(OutOfRange);
            }
            let k = (v / CHUNK).floor();
            let mut l = v - k * CHUNK;
            // 2^64 chunks: past the range whatever the start; below that the cast is exact.
            let mut kc: i128 = if k.abs() < 18_446_744_073_709_551_616.0 { k as i128 } else { return Err(OutOfRange) };
            if l >= CHUNK {
                l -= CHUNK;
                kc += 1;
            }
            let c = self.chunk[a] as i128 + kc;
            if c <= RESERVED_CHUNK as i128 || c > i64::MAX as i128 {
                return Err(OutOfRange);
            }
            out.chunk[a] = c as i64;
            out.local[a] = canon(l);
        }
        Ok(out)
    }

    /// `self` moved by `-d`.
    pub fn checked_sub(self, d: Offset) -> Result<Pos, OutOfRange> {
        self.checked_add(-d)
    }

    /// Squared distance in metres.
    pub fn dist2(&self, other: &Pos) -> f64 {
        (*self - *other).length2()
    }

    /// The canonical encoding (world-model.md 4.5): per axis, the chunk index as a zigzag LEB128 varint, then the
    /// offset's 8 little-endian bytes. One encoding per position.
    pub fn encode(&self, out: &mut Vec<u8>) {
        for a in 0..3 {
            let c = self.chunk[a] as i128;
            let mut z = ((c << 1) ^ (c >> 127)) as u128; // zigzag
            loop {
                let b = (z & 0x7f) as u8;
                z >>= 7;
                if z == 0 {
                    out.push(b);
                    break;
                }
                out.push(b | 0x80);
            }
            out.extend_from_slice(&self.local[a].to_le_bytes());
        }
    }
}

/// Total: the chunk difference is taken in `i128`, then rounded once.
impl core::ops::Sub for Pos {
    type Output = Offset;
    fn sub(self, other: Pos) -> Offset {
        let mut d = [0.0; 3];
        for a in 0..3 {
            let dc = self.chunk[a] as i128 - other.chunk[a] as i128;
            let dl = self.local[a] - other.local[a];
            d[a] = canon((dc as f64) * CHUNK + dl);
        }
        Offset(d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_line_is_plain_blake3_of_the_origin_entry() {
        assert_eq!(LineId::origin().0, Hash(*blake3::hash(b"pe.origin.v1").as_bytes()));
        // world-model's pinned value (9a570a9c...db9e), so a change to the origin bytes fails here too.
        let h = LineId::origin().0.0;
        assert_eq!((h[0], h[1], h[2], h[3]), (0x9a, 0x57, 0x0a, 0x9c));
        assert_eq!((h[30], h[31]), (0xdb, 0x9e));
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// Golden vectors, produced by WS-world's pe-core at world/core-v1 426d8420 (not by these copies). When pe-seams
    /// switches to re-exporting pe-core's types, this test must still pass unchanged, so the copies cannot drift.
    #[test]
    fn golden_vectors_match_pe_core_426d8420() {
        let line = LineId::origin();
        let key = MemberKey([0x11; 32]);
        let chain = ChainId::new(line, key, [0x22; 32]);
        assert_eq!(hex(&line.0.0), "9a570a9c46078da1b28a58daddf02dcea9c8bb31f4efc8ab61089b552b1ddb9e");
        assert_eq!(hex(&chain.0.0), "f9b15cc973e80406d2712f240a1d76008f015c16ca0f86b5f4bbde52c6869ac9");
        assert_eq!(format!("{:032x}", ObjectId::created(line, key, chain, 7, 3).0), "7b1e6a231cb93972bbf8ecaab2238c94");
        assert_eq!(
            format!("{:032x}", ObjectId::spawned(line, ObjectId(0x1234), Frame(99), 2).0),
            "6c8e7703bd9c45f9428b175321a10c71"
        );
        assert_eq!(format!("{:032x}", ObjectId::hand(line, key, 1, Frame(5)).0), "95b6a39b914e2646d926443016d398bd");
        let p = Pos::ORIGIN.checked_add(Offset([-1500.25, 0.5, 3_000_000.0])).unwrap();
        let mut out = Vec::new();
        p.encode(&mut out);
        assert_eq!(hex(&out), "0300000000001e814000000000000000e03fe22d0000000000008640");
        let far = Pos::at_chunk([1 << 40, -7, i64::MAX]).unwrap().checked_add(Offset([0.125, 1023.5, 0.0])).unwrap();
        let mut out = Vec::new();
        far.encode(&mut out);
        assert_eq!(hex(&out), "808080808040000000000000c03f0d0000000000fc8f40feffffffffffffffff010000000000000000");
        let d = far - p;
        assert_eq!(
            [d.0[0].to_bits(), d.0[1].to_bits(), d.0[2].to_bits()],
            [0x4310000000001772, 0xc0b8010000000000, 0x447ffffffffffffd]
        );
        assert_eq!(
            hex(&Hash::of("pe.test", b"golden").0),
            "656fdcd17676e327ae73607c0da58259c4fd15bbc98b0dd7a1b78cd90cbcf802"
        );
    }

    #[test]
    fn contexts_separate_hashes() {
        assert_ne!(Hash::of("pe.a", b"x"), Hash::of("pe.b", b"x"));
        assert_ne!(Hash::of("pe.a", b"x"), Hash::plain(b"x"));
    }

    #[test]
    fn pos_moves_and_subtracts_exactly() {
        let p = Pos::ORIGIN.checked_add(Offset([1500.5, -0.25, 3.0])).unwrap();
        assert_eq!(p - Pos::ORIGIN, Offset([1500.5, -0.25, 3.0]));
        let far = Pos::at_chunk([1 << 40, 0, -(1 << 40)]).unwrap();
        let near = far.checked_add(Offset([0.5, 0.0, 0.0])).unwrap();
        assert_eq!(near - far, Offset([0.5, 0.0, 0.0]));
        assert_eq!(Pos::ORIGIN.checked_add(Offset([f64::NAN, 0.0, 0.0])), Err(OutOfRange));
        assert_eq!(Pos::at_chunk([i64::MIN, 0, 0]), Err(OutOfRange));
        let top = Pos::at_chunk([i64::MAX, 0, 0]).unwrap();
        assert_eq!(top.checked_add(Offset([CHUNK, 0.0, 0.0])), Err(OutOfRange));
    }
}

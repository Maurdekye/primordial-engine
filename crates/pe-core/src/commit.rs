//! 256-bit commitments (world-model.md 6.1): BLAKE3, the only hash that may appear in a cut, a vote, a root or an
//! id. XXH64 ([`crate::hash`]) stays for non-commitment uses only.
//!
//! Every use is domain-separated: [`Hasher::new`] takes a context string (`"pe.object"`, `"pe.leaf"`, ...), which
//! is BLAKE3's key-derivation mode, so a hash made for one purpose can never equal one made for another. The spec's
//! notation `BLAKE3("pe.object" ‖ a ‖ b)` means `Hasher::new("pe.object")` fed `a` then `b`. Every field fed in is
//! fixed-length or length-prefixed by the caller, so the encoding is unambiguous.

use core::fmt;

/// A 256-bit BLAKE3 hash.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Hash(pub [u8; 32]);

impl Hash {
    pub const ZERO: Hash = Hash([0; 32]);

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The hash of `bytes` under the context `context`.
    pub fn of(context: &str, bytes: &[u8]) -> Hash {
        let mut h = Hasher::new(context);
        h.write(bytes);
        h.finish()
    }

    /// The hash of plain bytes with no context: only for content addressing (a blob's name, 3.4) and the origin
    /// entry, whose definitions are plain BLAKE3.
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

/// A domain-separated streaming BLAKE3 hasher.
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

    pub fn write_u16(&mut self, v: u16) -> &mut Self {
        self.write(&v.to_le_bytes())
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

    /// Writes a length prefix (`u64` LE) and then the bytes, for variable-length fields.
    pub fn write_len_prefixed(&mut self, bytes: &[u8]) -> &mut Self {
        self.write_u64(bytes.len() as u64);
        self.write(bytes)
    }

    pub fn finish(&self) -> Hash {
        Hash(*self.0.finalize().as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contexts_separate() {
        assert_ne!(Hash::of("pe.a", b"x"), Hash::of("pe.b", b"x"));
        assert_ne!(Hash::of("pe.a", b"x"), Hash::plain(b"x"));
        let mut h = Hasher::new("pe.a");
        h.write(b"x");
        assert_eq!(h.finish(), Hash::of("pe.a", b"x"));
    }

    #[test]
    fn plain_matches_reference_vector() {
        // BLAKE3 of the empty input, from the BLAKE3 test vectors.
        assert_eq!(Hash::plain(b"").to_string(), "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262");
    }
}

//! Seam S1, entries: the signed envelope everything else carries (plan 3.2; world-model.md 2, 9.1).
//!
//! - **The envelope** ([`Entry`]): the line it was made on (bound into the signature, so an entry cannot be replayed
//!   into an unrelated world), the author's key, the chain id and sequence number, the previous entry's id, the
//!   domain, the frame stamp, the class and the payload. A chain's first entry also carries the nonce its chain id
//!   derives from.
//! - **The payload is opaque here.** pe-world owns the `Input` and `Edit` codecs and the storage side of
//!   `Governance`; pe-agree owns `Protocol` and never decodes the others.
//! - **Signing** is an Ed25519 signature by the author's key over [`Entry::signing_bytes`], behind the [`Signer`]
//!   and [`Verifier`] traits. pe-net-crypto implements them (WS-net); [`crate::test_util`] has a stand-in for tests.
//! - **The entry id** ([`Entry::id`]) is the hash of the signing bytes, so it is fixed before the entry is signed and
//!   does not depend on the signature's bytes.
//!
//! **Provisional in v0:** the byte layout of [`Entry::signing_bytes`] and [`SignedEntry::encode`] (version 1), and
//! the payload limit. WS-agree (@world-chunks) may change them in v1 through WS-seams; until then they are what every
//! crate uses. Branch and merge entries (M2), which sit outside every chain, come with v1.

use bytes::Bytes;

use crate::ids::{ChainId, EntryRef, Frame, Hash, Hasher, LineId, MemberKey};

/// The largest payload an entry may carry: a transaction's 1 MiB of payload (world-model.md 7.1), plus room for its
/// header. Blobs travel apart, by hash, on the Bulk lane.
pub const MAX_PAYLOAD: usize = (1 << 20) + 4096;

/// The format version of [`Entry::signing_bytes`] and [`SignedEntry::encode`].
pub const ENTRY_FORMAT: u8 = 1;

/// The context of the entry id's hash.
const ID_CONTEXT: &str = "pe.entry.v1";
/// Prefixed to the signing bytes before they are signed, so an entry signature can never be read as a signature
/// over anything else the member key signs (cuts, votes, the TLS handshake).
pub const SIGNING_PREFIX: &[u8] = b"pe.sig.entry.v1\0";

/// A simulation domain (plan 3.2). M1 uses one value per world; M4's regions give each contact group its own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DomainId(pub u64);

/// What an entry is (plan 3.2). pe-agree orders all four and decodes only `Protocol`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Class {
    /// A player's input: hand rows, grabs and releases, pushes (world-model.md 8).
    Input = 0,
    /// A transaction of edits (world-model.md 7), including a script commit.
    Edit = 1,
    /// Roles, admission and removal, settings (world-model.md 5.4).
    Governance = 2,
    /// Agreement's own entries: joins, leaves, and what pe-agree records in the log.
    Protocol = 3,
}

impl Class {
    pub fn from_u8(v: u8) -> Option<Class> {
        Some(match v {
            0 => Class::Input,
            1 => Class::Edit,
            2 => Class::Governance,
            3 => Class::Protocol,
            _ => return None,
        })
    }
}

/// An Ed25519 signature.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Signature(pub [u8; 64]);

impl core::fmt::Debug for Signature {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Signature(")?;
        for b in &self.0[..6] {
            write!(f, "{b:02x}")?;
        }
        write!(f, "…)")
    }
}

/// The envelope, before signing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The line the entry was made on. It binds the entry to that line's history.
    pub line: LineId,
    pub author: MemberKey,
    pub chain: ChainId,
    /// The position in the author's chain, from 1.
    pub seq: u64,
    /// The id of the chain's previous entry; `None` exactly at seq 1.
    pub prev: Option<EntryRef>,
    /// At seq 1 only: the nonce the chain id derives from, `chain = ChainId::new(line, author, nonce)`.
    pub nonce: Option<[u8; 32]>,
    pub domain: DomainId,
    /// The frame the author stamped it for. pe-log orders by stamp, then by the seeded rank (D37).
    pub stamp: Frame,
    pub class: Class,
    pub payload: Bytes,
}

/// Why bytes are not a well-formed entry. Decoding never panics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryError {
    /// Shorter than its fields say.
    Truncated,
    /// Bytes after the end.
    Trailing,
    /// A format version this build does not read.
    Version(u8),
    UnknownClass(u8),
    /// A flag byte other than 0 or 1.
    BadFlag,
    PayloadTooLarge(usize),
    /// seq 0, a previous id at seq 1 or none after it, or a nonce anywhere but seq 1.
    ChainShape,
    /// At seq 1, the chain id is not derived from (line, author, nonce).
    ChainId,
}

impl Entry {
    /// The canonical bytes the author signs and the id hashes (format 1, little-endian):
    /// `ENTRY_FORMAT u8 ‖ line 32 ‖ author 32 ‖ chain 32 ‖ seq u64 ‖ prev flag u8 [‖ prev 32] ‖ nonce flag u8
    /// [‖ nonce 32] ‖ domain u64 ‖ stamp u64 ‖ class u8 ‖ payload length u32 ‖ payload`.
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(200 + self.payload.len());
        out.push(ENTRY_FORMAT);
        out.extend_from_slice(&self.line.0.0);
        out.extend_from_slice(&self.author.0);
        out.extend_from_slice(&self.chain.0.0);
        out.extend_from_slice(&self.seq.to_le_bytes());
        match &self.prev {
            Some(p) => {
                out.push(1);
                out.extend_from_slice(&p.0.0);
            }
            None => out.push(0),
        }
        match &self.nonce {
            Some(n) => {
                out.push(1);
                out.extend_from_slice(n);
            }
            None => out.push(0),
        }
        out.extend_from_slice(&self.domain.0.to_le_bytes());
        out.extend_from_slice(&self.stamp.0.to_le_bytes());
        out.push(self.class as u8);
        out.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    /// The entry's id: `BLAKE3("pe.entry.v1" ‖ signing bytes)`.
    pub fn id(&self) -> EntryRef {
        let mut h = Hasher::new(ID_CONTEXT);
        h.write(&self.signing_bytes());
        EntryRef(h.finish())
    }

    /// The chain rules S1 checks alone, without the rest of the chain: seq from 1, `prev` exactly after seq 1, the
    /// nonce exactly at seq 1, and the chain id derived from it there. Linking to the previous entry and the
    /// monotone stamp are pe-agree's checks, which need the chain.
    pub fn check_shape(&self) -> Result<(), EntryError> {
        if self.payload.len() > MAX_PAYLOAD {
            return Err(EntryError::PayloadTooLarge(self.payload.len()));
        }
        match (self.seq, &self.prev, &self.nonce) {
            (0, _, _) => Err(EntryError::ChainShape),
            (1, None, Some(nonce)) => {
                if ChainId::new(self.line, self.author, *nonce) == self.chain {
                    Ok(())
                } else {
                    Err(EntryError::ChainId)
                }
            }
            (1, _, _) => Err(EntryError::ChainShape),
            (_, Some(_), None) => Ok(()),
            _ => Err(EntryError::ChainShape),
        }
    }

    /// Signs the entry with the author's key. The signer's key must be the author's.
    pub fn sign(self, signer: &dyn Signer) -> SignedEntry {
        debug_assert_eq!(signer.key(), self.author, "an entry is signed by its author");
        let mut msg = SIGNING_PREFIX.to_vec();
        msg.extend_from_slice(&self.signing_bytes());
        let sig = signer.sign(&msg);
        SignedEntry { entry: self, sig }
    }
}

/// An entry with its author's signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedEntry {
    pub entry: Entry,
    pub sig: Signature,
}

impl SignedEntry {
    pub fn id(&self) -> EntryRef {
        self.entry.id()
    }

    /// Whether the signature is the author's over the signing bytes. Strict verification: a small-order key, such
    /// as world logic's all-zero key, never verifies.
    pub fn verify(&self, verifier: &dyn Verifier) -> bool {
        let mut msg = SIGNING_PREFIX.to_vec();
        msg.extend_from_slice(&self.entry.signing_bytes());
        verifier.verify(&self.entry.author, &msg, &self.sig)
    }

    /// The wire and storage form: the signing bytes, then the 64-byte signature.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = self.entry.signing_bytes();
        out.extend_from_slice(&self.sig.0);
        out
    }

    /// The inverse of [`SignedEntry::encode`]. Total: any bytes give an entry or an error, never a panic. It checks
    /// the envelope's shape ([`Entry::check_shape`]); it does not verify the signature.
    pub fn decode(bytes: &[u8]) -> Result<SignedEntry, EntryError> {
        let mut r = Reader(bytes);
        let version = r.u8()?;
        if version != ENTRY_FORMAT {
            return Err(EntryError::Version(version));
        }
        let line = LineId(Hash(r.array()?));
        let author = MemberKey(r.array()?);
        let chain = ChainId(Hash(r.array()?));
        let seq = r.u64()?;
        let prev = if r.flag()? { Some(EntryRef(Hash(r.array()?))) } else { None };
        let nonce = if r.flag()? { Some(r.array()?) } else { None };
        let domain = DomainId(r.u64()?);
        let stamp = Frame(r.u64()?);
        let class_byte = r.u8()?;
        let class = Class::from_u8(class_byte).ok_or(EntryError::UnknownClass(class_byte))?;
        let len = r.u32()? as usize;
        if len > MAX_PAYLOAD {
            return Err(EntryError::PayloadTooLarge(len));
        }
        let payload = Bytes::copy_from_slice(r.take(len)?);
        let sig = Signature(r.array()?);
        if !r.0.is_empty() {
            return Err(EntryError::Trailing);
        }
        let entry = Entry { line, author, chain, seq, prev, nonce, domain, stamp, class, payload };
        entry.check_shape()?;
        Ok(SignedEntry { entry, sig })
    }
}

/// Signs with one member key. pe-net-crypto holds the secret (WS-net); pe-agree's `submit` calls it.
pub trait Signer: Send + Sync {
    fn key(&self) -> MemberKey;
    fn sign(&self, msg: &[u8]) -> Signature;
}

/// Checks a signature by any member key: Ed25519, strict (pe-net-crypto).
pub trait Verifier: Send + Sync {
    fn verify(&self, key: &MemberKey, msg: &[u8], sig: &Signature) -> bool;
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], EntryError> {
        if self.0.len() < n {
            return Err(EntryError::Truncated);
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], EntryError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, EntryError> {
        Ok(self.take(1)?[0])
    }

    fn flag(&mut self) -> Result<bool, EntryError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(EntryError::BadFlag),
        }
    }

    fn u32(&mut self) -> Result<u32, EntryError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, EntryError> {
        Ok(u64::from_le_bytes(self.array()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{EntryBuilder, TestKeys};

    #[test]
    fn encode_decode_round_trip_and_verify() {
        let keys = TestKeys::new(7, 2);
        let mut b = EntryBuilder::new(&keys, 0);
        let first = b.next(Class::Edit, Frame(10), Bytes::from_static(b"payload one"));
        let second = b.next(Class::Input, Frame(11), Bytes::from_static(b"two"));
        assert_eq!(first.entry.seq, 1);
        assert_eq!(second.entry.prev, Some(first.id()));
        for e in [&first, &second] {
            let back = SignedEntry::decode(&e.encode()).unwrap();
            assert_eq!(&back, e);
            assert!(back.verify(&keys));
        }
    }

    #[test]
    fn a_changed_byte_changes_the_id_and_breaks_the_signature() {
        let keys = TestKeys::new(1, 2);
        let mut b = EntryBuilder::new(&keys, 0);
        let e = b.next(Class::Edit, Frame(3), Bytes::from_static(b"x"));
        let mut forged = e.clone();
        forged.entry.author = keys.key(1);
        assert!(!forged.verify(&keys));
        let mut moved = e.clone();
        moved.entry.stamp = Frame(2);
        assert_ne!(moved.id(), e.id());
        assert!(!moved.verify(&keys));
        let mut other_line = e.clone();
        other_line.entry.line = LineId(Hash::of("pe.test", b"another line"));
        assert!(!other_line.verify(&keys));
    }

    #[test]
    fn decoding_is_total() {
        let keys = TestKeys::new(2, 1);
        let mut b = EntryBuilder::new(&keys, 0);
        let bytes = b.next(Class::Governance, Frame(0), Bytes::from_static(b"abc")).encode();
        for cut in 0..bytes.len() {
            assert!(SignedEntry::decode(&bytes[..cut]).is_err());
        }
        let mut long = bytes.clone();
        long.push(0);
        assert_eq!(SignedEntry::decode(&long), Err(EntryError::Trailing));
        let mut bad_class = bytes.clone();
        let class_at = bytes.len() - 64 - 3 - 4 - 1;
        bad_class[class_at] = 9;
        assert_eq!(SignedEntry::decode(&bad_class), Err(EntryError::UnknownClass(9)));
        let mut version = bytes;
        version[0] = 2;
        assert_eq!(SignedEntry::decode(&version), Err(EntryError::Version(2)));
    }

    #[test]
    fn chain_shape_is_checked() {
        let keys = TestKeys::new(3, 1);
        let mut b = EntryBuilder::new(&keys, 0);
        let first = b.next(Class::Input, Frame(1), Bytes::new());
        let mut no_nonce = first.entry.clone();
        no_nonce.nonce = None;
        assert_eq!(no_nonce.check_shape(), Err(EntryError::ChainShape));
        let mut wrong_chain = first.entry.clone();
        wrong_chain.nonce = Some([9; 32]);
        assert_eq!(wrong_chain.check_shape(), Err(EntryError::ChainId));
        let second = b.next(Class::Input, Frame(1), Bytes::new());
        let mut nonce_later = second.entry.clone();
        nonce_later.nonce = Some([0; 32]);
        assert_eq!(nonce_later.check_shape(), Err(EntryError::ChainShape));
        let mut seq0 = second.entry;
        seq0.seq = 0;
        assert_eq!(seq0.check_shape(), Err(EntryError::ChainShape));
    }
}

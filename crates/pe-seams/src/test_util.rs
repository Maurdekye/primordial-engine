//! Test helpers (feature `test-util`; never in a shipped build).
//!
//! - [`TestKeys`]: deterministic member keys from a seed, with a **stand-in signature** that is not a signature
//!   scheme: it is a keyed hash that only the holder of the table can check. It makes "forged author" and "wrong
//!   line" fail exactly as real signatures do, at hash speed, for in-process tests. Real Ed25519 is pe-net-crypto's.
//! - [`EntryBuilder`]: well-formed chains for one member, signed.
//! - [`Corrupt`]: the replica's test-only fault injection (DESIGN.md 10, S2.3).

use bytes::Bytes;

use crate::entry::{Class, DomainId, Entry, Signature, SignedEntry, Signer, Verifier};
use crate::ids::{ChainId, EntryRef, Frame, Hash, Hasher, LineId, MemberKey, ObjectId};

/// `n` deterministic members from `seed`.
#[derive(Clone, Debug)]
pub struct TestKeys {
    seed: u64,
    keys: Vec<(MemberKey, Hash)>,
}

fn derive(context: &str, seed: u64, i: u32) -> Hash {
    let mut h = Hasher::new(context);
    h.write_u64(seed).write_u32(i);
    h.finish()
}

impl TestKeys {
    pub fn new(seed: u64, n: u32) -> TestKeys {
        let keys =
            (0..n).map(|i| (MemberKey(derive("pe.test.key", seed, i).0), derive("pe.test.secret", seed, i))).collect();
        TestKeys { seed, keys }
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn key(&self, i: usize) -> MemberKey {
        self.keys[i].0
    }

    /// The members' keys, sorted.
    pub fn sorted_keys(&self) -> Vec<MemberKey> {
        let mut k: Vec<MemberKey> = self.keys.iter().map(|k| k.0).collect();
        k.sort();
        k
    }

    pub fn signer(&self, i: usize) -> TestSigner {
        TestSigner { key: self.keys[i].0, secret: self.keys[i].1 }
    }

    /// The nonce member `i`'s first chain starts with; `restart` > 0 gives the nonce of a chain started again.
    pub fn nonce(&self, i: usize, restart: u32) -> [u8; 32] {
        let mut h = Hasher::new("pe.test.nonce");
        h.write_u64(self.seed).write_u32(i as u32).write_u32(restart);
        h.finish().0
    }
}

fn stand_in_signature(secret: &Hash, msg: &[u8]) -> Signature {
    let mut a = Hasher::new("pe.test.sig.a");
    a.write_hash(secret).write(msg);
    let mut b = Hasher::new("pe.test.sig.b");
    b.write_hash(secret).write(msg);
    let mut out = [0u8; 64];
    out[..32].copy_from_slice(&a.finish().0);
    out[32..].copy_from_slice(&b.finish().0);
    Signature(out)
}

impl Verifier for TestKeys {
    fn verify(&self, key: &MemberKey, msg: &[u8], sig: &Signature) -> bool {
        match self.keys.iter().find(|k| &k.0 == key) {
            Some((_, secret)) => stand_in_signature(secret, msg) == *sig,
            None => false,
        }
    }
}

/// One member's stand-in signer.
#[derive(Clone, Debug)]
pub struct TestSigner {
    key: MemberKey,
    secret: Hash,
}

impl Signer for TestSigner {
    fn key(&self) -> MemberKey {
        self.key
    }

    fn sign(&self, msg: &[u8]) -> Signature {
        stand_in_signature(&self.secret, msg)
    }
}

/// Builds one member's chain: seq from 1, each entry naming the previous one.
#[derive(Clone, Debug)]
pub struct EntryBuilder {
    signer: TestSigner,
    line: LineId,
    domain: DomainId,
    nonce: [u8; 32],
    chain: ChainId,
    seq: u64,
    prev: Option<EntryRef>,
}

impl EntryBuilder {
    /// Member `i` of `keys`, on the origin line, domain 0.
    pub fn new(keys: &TestKeys, i: usize) -> EntryBuilder {
        EntryBuilder::on_line(keys, i, LineId::origin())
    }

    pub fn on_line(keys: &TestKeys, i: usize, line: LineId) -> EntryBuilder {
        let signer = keys.signer(i);
        let nonce = keys.nonce(i, 0);
        let chain = ChainId::new(line, signer.key, nonce);
        EntryBuilder { signer, line, domain: DomainId(0), nonce, chain, seq: 0, prev: None }
    }

    pub fn chain(&self) -> ChainId {
        self.chain
    }

    /// The next entry, unsigned, so a test can alter it before signing with [`EntryBuilder::sign`].
    pub fn next_unsigned(&mut self, class: Class, stamp: Frame, payload: Bytes) -> Entry {
        self.seq += 1;
        let e = Entry {
            line: self.line,
            author: self.signer.key,
            chain: self.chain,
            seq: self.seq,
            prev: self.prev,
            nonce: (self.seq == 1).then_some(self.nonce),
            domain: self.domain,
            stamp,
            class,
            payload,
        };
        self.prev = Some(e.id());
        e
    }

    /// The next entry, signed by this member.
    pub fn next(&mut self, class: Class, stamp: Frame, payload: Bytes) -> SignedEntry {
        let e = self.next_unsigned(class, stamp, payload);
        e.sign(&self.signer)
    }

    /// Sign any envelope with this member's key, well-formed or not.
    pub fn sign(&self, e: Entry) -> SignedEntry {
        let mut msg = crate::entry::SIGNING_PREFIX.to_vec();
        msg.extend_from_slice(&e.signing_bytes());
        let sig = self.signer.sign(&msg);
        SignedEntry { entry: e, sig }
    }
}

/// Test-only fault injection into a replica: change one field of one object without an entry, so its root differs
/// from everyone else's. Returns whether the object and field existed.
pub trait Corrupt {
    fn corrupt(&mut self, object: ObjectId, field: u32) -> bool;
}

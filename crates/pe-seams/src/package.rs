//! (S3 v1.15; M2 S7; divergence.md 9, package delta r2 Fix 1.) A cut-off member's package and the merge entry `M`
//! that brings it back.
//!
//! - **The segment.** From the trigger, each action a cut-off member B makes is a [`PackageIntent`], signed by B
//!   under [`INTENT_DOMAIN`] and numbered by its sequence in the segment. An intent is never an entry of B's ordinary
//!   chain: no cut includes one, and it executes only through `M`. [`SegmentId`] is unique per attempt: B, the live
//!   line, the base cut, a durable generation number, and the predecessor package's id.
//! - **The commitment.** At the seal, B signs a [`Commitment`] under [`COMMITMENT_DOMAIN`]: the segment, B's
//!   ordinary chain head at the seal, the intents' count, their highest stamp (`top`) and a hash over them. Its
//!   [`PackageId`] is the hash of the commitment's own bytes, so it binds the content, the head and the stamps. It
//!   doesn't cover the signature: a commitment signed again (a retry after a crash) keeps its id (@design-reviewer's
//!   L1 on row 4a plan r1).
//! - **`M` carries the signed commitment** ([`MergePayload`]), not just its id (@design-reviewer's H1). So every
//!   member, a non-voter or a later joiner included, decides `M`'s [`MergeOutcome`] from the final log and `M`
//!   alone. Only applying `M` needs the intents, which members fetch by the package's id from any holder and check
//!   against the commitment ([`verify_package`]).
//! - **`M`'s stamp is above every intent** in its package: an `M` stamped at or below `top` is refused
//!   ([`MergeRefusal::MBelowIntents`]).
//!
//! S3 decides each `M` at its final cut and reports it as [`crate::agree::AgreeEvent::Merged`], with the cut's frame
//! `F_M`. The application frame `A = F_M + D` and the order in which merges apply are pe-diverge's and pe-app's
//! (divergence.md 9.3a: `D` is a function of the final log at `F_M`).

use bytes::Bytes;

use crate::agree::{ChainHead, CutRef};
use crate::entry::{Class, Signature, SignedEntry, Signer, Verifier};
use crate::ids::{ChainId, EntryRef, Frame, Hash, Hasher, LineId, MemberKey};

/// The signing domain of a package intent: never an entry's, a vote's or a commitment's.
pub const INTENT_DOMAIN: &[u8] = b"pe.sig.package.intent.v1\0";
/// The signing domain of a package's commitment.
pub const COMMITMENT_DOMAIN: &[u8] = b"pe.sig.package.commitment.v1\0";
/// The tag a merge entry's payload starts with, then its version ([`MERGE_VERSION`]); ends in `/` as the governance
/// tags do.
pub const MERGE: &[u8] = b"pe.agree.merge/";
/// The merge payload's format version.
pub const MERGE_VERSION: u8 = 1;
/// The most intents one package holds. A decoder refuses more, so a member's bytes can't make it allocate past it.
pub const MAX_INTENTS: u32 = 1 << 16;

/// A package's id: the hash of its commitment's bytes ([`Commitment::id`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PackageId(pub Hash);

/// One attempt's segment (module doc): unique per attempt, since two valid cycles can share B, the line and the base.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SegmentId {
    /// B, the member that was cut off.
    pub author: MemberKey,
    /// The live line the package returns to (at n = 2, L').
    pub line: LineId,
    /// The base `Cb`: B's last verified final cut of `line` before the trigger (at n = 2, L''s genesis).
    pub base: CutRef,
    /// B's durable generation number: one more for each attempt.
    pub generation: u64,
    /// The predecessor package, if this is a second one (r2, "Two successive packages").
    pub pred: Option<PackageId>,
}

impl SegmentId {
    /// The hash an intent names its segment by.
    pub fn hash(&self) -> Hash {
        let mut h = Hasher::new("pe.package.segment");
        write_segment(&mut h, self);
        h.finish()
    }
}

/// One action B made while cut off (module doc). Same-frame intents are ordered by `seq`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageIntent {
    /// [`SegmentId::hash`] of its segment.
    pub segment: Hash,
    /// Its place in the segment, from 0.
    pub seq: u32,
    /// The frame it is stamped for (r2 Fix 2's rule, B's side).
    pub stamp: Frame,
    pub class: Class,
    pub payload: Bytes,
}

impl PackageIntent {
    /// The bytes it is signed and named by.
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(32 + 4 + 8 + 1 + 4 + self.payload.len());
        b.extend_from_slice(&self.segment.0);
        b.extend_from_slice(&self.seq.to_le_bytes());
        b.extend_from_slice(&self.stamp.0.to_le_bytes());
        b.push(self.class as u8);
        b.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());
        b.extend_from_slice(&self.payload);
        b
    }

    pub fn sign(self, signer: &dyn Signer) -> SignedIntent {
        let sig = signer.sign(&[INTENT_DOMAIN, &self.signing_bytes()].concat());
        SignedIntent { intent: self, sig }
    }
}

/// An intent with B's signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedIntent {
    pub intent: PackageIntent,
    pub sig: Signature,
}

impl SignedIntent {
    /// Its name in the commitment's hash: over the signed bytes, not the signature.
    pub fn id(&self) -> Hash {
        Hash::of("pe.package.intent.id", &self.intent.signing_bytes())
    }

    pub fn verify(&self, author: &MemberKey, verifier: &dyn Verifier) -> bool {
        verifier.verify(author, &[INTENT_DOMAIN, &self.intent.signing_bytes()].concat(), &self.sig)
    }

    /// The wire form: the signing bytes, then the 64-byte signature.
    pub fn encode(&self) -> Vec<u8> {
        let mut b = self.intent.signing_bytes();
        b.extend_from_slice(&self.sig.0);
        b
    }

    /// [`SignedIntent::encode`]'s inverse, with what follows it; never panics.
    pub fn decode(b: &[u8]) -> Option<(SignedIntent, &[u8])> {
        let mut r = Reader(b);
        let segment = Hash(r.array()?);
        let seq = u32::from_le_bytes(r.array()?);
        let stamp = Frame(u64::from_le_bytes(r.array()?));
        let class = Class::from_u8(r.take(1)?[0])?;
        let len = u32::from_le_bytes(r.array()?) as usize;
        if len > crate::entry::MAX_PAYLOAD {
            return None;
        }
        let payload = Bytes::copy_from_slice(r.take(len)?);
        let sig = Signature(r.array()?);
        Some((SignedIntent { intent: PackageIntent { segment, seq, stamp, class, payload }, sig }, r.0))
    }
}

/// The hash over a package's intents, in `seq` order, that the commitment names.
pub fn intents_root(intents: &[SignedIntent]) -> Hash {
    let mut h = Hasher::new("pe.package.intents");
    h.write_u64(intents.len() as u64);
    for i in intents {
        h.write_hash(&i.id());
    }
    h.finish()
}

/// What B signs at the seal (module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Commitment {
    pub segment: SegmentId,
    /// B's ordinary chain head at the seal (none if B's chain had no entry yet).
    pub head: Option<ChainHead>,
    /// The number of intents.
    pub count: u32,
    /// The highest intent stamp: `M` must be stamped above it.
    pub top: Frame,
    /// [`intents_root`] of the intents.
    pub intents: Hash,
}

impl Commitment {
    pub fn bytes(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(256);
        let s = &self.segment;
        b.extend_from_slice(&s.author.0);
        b.extend_from_slice(&s.line.0.0);
        put_cut_ref(&mut b, &s.base);
        b.extend_from_slice(&s.generation.to_le_bytes());
        put_opt_hash(&mut b, s.pred.map(|p| p.0));
        match &self.head {
            None => b.push(0),
            Some(h) => {
                b.push(1);
                b.extend_from_slice(&h.chain.0.0);
                b.extend_from_slice(&h.author.0);
                b.extend_from_slice(&h.seq.to_le_bytes());
                b.extend_from_slice(&h.entry.0.0);
            }
        }
        b.extend_from_slice(&self.count.to_le_bytes());
        b.extend_from_slice(&self.top.0.to_le_bytes());
        b.extend_from_slice(&self.intents.0);
        b
    }

    /// The package's id: the hash of these bytes, without the signature (module doc).
    pub fn id(&self) -> PackageId {
        PackageId(Hash::of("pe.package.id", &self.bytes()))
    }

    pub fn sign(self, signer: &dyn Signer) -> SignedCommitment {
        let sig = signer.sign(&[COMMITMENT_DOMAIN, &self.bytes()].concat());
        SignedCommitment { p: self, sig }
    }

    fn decode(b: &[u8]) -> Option<(Commitment, &[u8])> {
        let mut r = Reader(b);
        let author = MemberKey(r.array()?);
        let line = LineId(Hash(r.array()?));
        let base = get_cut_ref(&mut r)?;
        let generation = u64::from_le_bytes(r.array()?);
        let pred = get_opt_hash(&mut r)?.map(PackageId);
        let head = match r.take(1)?[0] {
            0 => None,
            1 => Some(ChainHead {
                chain: ChainId(Hash(r.array()?)),
                author: MemberKey(r.array()?),
                seq: u64::from_le_bytes(r.array()?),
                entry: EntryRef(Hash(r.array()?)),
            }),
            _ => return None,
        };
        let count = u32::from_le_bytes(r.array()?);
        let top = Frame(u64::from_le_bytes(r.array()?));
        let intents = Hash(r.array()?);
        let segment = SegmentId { author, line, base, generation, pred };
        Some((Commitment { segment, head, count, top, intents }, r.0))
    }
}

/// A commitment with B's signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SignedCommitment {
    pub p: Commitment,
    pub sig: Signature,
}

impl SignedCommitment {
    pub fn id(&self) -> PackageId {
        self.p.id()
    }

    /// Whether the segment's author signed it.
    pub fn verify(&self, verifier: &dyn Verifier) -> bool {
        verifier.verify(&self.p.segment.author, &[COMMITMENT_DOMAIN, &self.p.bytes()].concat(), &self.sig)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut b = self.p.bytes();
        b.extend_from_slice(&self.sig.0);
        b
    }

    /// [`SignedCommitment::encode`]'s inverse, with what follows it; never panics.
    pub fn decode(b: &[u8]) -> Option<(SignedCommitment, &[u8])> {
        let (p, rest) = Commitment::decode(b)?;
        let mut r = Reader(rest);
        let sig = Signature(r.array()?);
        Some((SignedCommitment { p, sig }, r.0))
    }
}

/// Why a package's intents don't match its commitment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackageError {
    /// The commitment isn't signed by its segment's author.
    BadCommitment,
    /// The intents' count differs from the commitment's, or is over [`MAX_INTENTS`].
    Count,
    /// An intent names another segment, is out of `seq` order, or isn't signed by the segment's author.
    BadIntent(u32),
    /// The highest intent stamp isn't the commitment's `top`.
    Top,
    /// The intents' hash isn't the commitment's.
    Root,
}

/// Whether `intents` are exactly the package `p` commits to: what a member checks a fetched package by, whoever
/// served it.
pub fn verify_package(
    p: &SignedCommitment,
    intents: &[SignedIntent],
    verifier: &dyn Verifier,
) -> Result<(), PackageError> {
    if !p.verify(verifier) {
        return Err(PackageError::BadCommitment);
    }
    if p.p.count > MAX_INTENTS || intents.len() != p.p.count as usize {
        return Err(PackageError::Count);
    }
    let seg = p.p.segment.hash();
    for (k, i) in intents.iter().enumerate() {
        if i.intent.segment != seg || i.intent.seq as usize != k || !i.verify(&p.p.segment.author, verifier) {
            return Err(PackageError::BadIntent(k as u32));
        }
    }
    let top = intents.iter().map(|i| i.intent.stamp).max().unwrap_or(Frame(0));
    if top != p.p.top {
        return Err(PackageError::Top);
    }
    if intents_root(intents) != p.p.intents {
        return Err(PackageError::Root);
    }
    Ok(())
}

/// A merge entry's payload (`M`, a [`Class::Protocol`] entry by the segment's author): the signed commitment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MergePayload {
    pub commitment: SignedCommitment,
}

impl MergePayload {
    /// The payload: tag, version, then the signed commitment.
    pub fn encode(&self) -> Bytes {
        let mut p = MERGE.to_vec();
        p.push(MERGE_VERSION);
        p.extend_from_slice(&self.commitment.encode());
        Bytes::from(p)
    }

    /// The merge payload in `payload`: `None` unless it starts with [`MERGE`]; `Some(None)` when it does but isn't a
    /// well-formed one of this version.
    pub fn decode(payload: &[u8]) -> Option<Option<MergePayload>> {
        let rest = payload.strip_prefix(MERGE)?;
        Some(match rest.split_first() {
            Some((&MERGE_VERSION, rest)) => match SignedCommitment::decode(rest) {
                Some((commitment, [])) => Some(MergePayload { commitment }),
                _ => None,
            },
            _ => None,
        })
    }

    /// The merge payload `e` carries: `None` if `e` isn't one; `Some(None)` if it is a malformed one.
    pub fn of(e: &SignedEntry) -> Option<Option<MergePayload>> {
        if e.entry.class != Class::Protocol {
            return None;
        }
        MergePayload::decode(&e.entry.payload)
    }
}

/// S3's verdict on a merge entry at its final cut (rules 1-5 of model v2's layer 2; @harness-eng's list).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeOutcome {
    /// The package merges: it applies at `A` (pe-diverge, pe-app).
    Accepted,
    /// The same package merged at an earlier final cut: a retry, a no-op.
    AlreadyMerged,
    Refused(MergeRefusal),
}

/// Why S3 refuses a merge entry. Each is decided from the final log and the entry alone, so every member reaches the
/// same verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeRefusal {
    /// `M` is stamped at or below its package's highest intent stamp.
    MBelowIntents,
    /// Another package already merged for this segment.
    SecondPackage,
    /// The predecessor package isn't final and accepted at or below this cut. The id and segment stay free.
    PredecessorNotMerged,
    /// `M`'s author isn't its segment's, isn't a member, or the commitment's signature doesn't verify; or the payload
    /// is malformed.
    NotAuthorized,
    /// The segment's base isn't a final cut of this line that members still keep the history from
    /// (@design-reviewer's M2 on row 4a plan r1).
    BaseNotKept,
    /// The final log from the base to `M`'s cut is over the protocol's history budget (the coordinator's ruling C:
    /// a constant over the final entries' bytes, which every member counts alike).
    HistoryBudget,
}

fn write_segment(h: &mut Hasher, s: &SegmentId) {
    h.write(&s.author.0).write_hash(&s.line.0);
    h.write_hash(&s.base.line.0).write_u64(s.base.height).write_u64(s.base.frame.0);
    h.write_u64(s.generation);
    match s.pred {
        None => h.write_u64(0),
        Some(p) => h.write_u64(1).write_hash(&p.0),
    };
}

fn put_cut_ref(b: &mut Vec<u8>, c: &CutRef) {
    b.extend_from_slice(&c.line.0.0);
    b.extend_from_slice(&c.height.to_le_bytes());
    b.extend_from_slice(&c.frame.0.to_le_bytes());
}

fn get_cut_ref(r: &mut Reader<'_>) -> Option<CutRef> {
    Some(CutRef {
        line: LineId(Hash(r.array()?)),
        height: u64::from_le_bytes(r.array()?),
        frame: Frame(u64::from_le_bytes(r.array()?)),
    })
}

fn put_opt_hash(b: &mut Vec<u8>, h: Option<Hash>) {
    match h {
        None => b.push(0),
        Some(h) => {
            b.push(1);
            b.extend_from_slice(&h.0);
        }
    }
}

fn get_opt_hash(r: &mut Reader<'_>) -> Option<Option<Hash>> {
    match r.take(1)?[0] {
        0 => Some(None),
        1 => Some(Some(Hash(r.array()?))),
        _ => None,
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }

    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TestKeys;

    fn package(keys: &TestKeys, n: u32) -> (SignedCommitment, Vec<SignedIntent>) {
        let b = keys.signer(0);
        let line = LineId::origin();
        let segment = SegmentId {
            author: keys.key(0),
            line,
            base: CutRef { line, height: 4, frame: Frame(24) },
            generation: 1,
            pred: None,
        };
        let seg = segment.hash();
        let intents: Vec<SignedIntent> = (0..n)
            .map(|k| {
                PackageIntent {
                    segment: seg,
                    seq: k,
                    stamp: Frame(30 + u64::from(k) / 2),
                    class: Class::Input,
                    payload: Bytes::from(vec![k as u8; 8]),
                }
                .sign(&b)
            })
            .collect();
        let top = intents.iter().map(|i| i.intent.stamp).max().unwrap_or(Frame(0));
        let p = Commitment { segment, head: None, count: n, top, intents: intents_root(&intents) }.sign(&b);
        (p, intents)
    }

    #[test]
    fn a_package_verifies_and_round_trips() {
        let keys = TestKeys::new(1, 2);
        let (p, intents) = package(&keys, 5);
        assert_eq!(verify_package(&p, &intents, &keys), Ok(()));
        let bytes = p.encode();
        let (q, rest) = SignedCommitment::decode(&bytes).unwrap();
        assert!(rest.is_empty());
        assert_eq!(q, p);
        for i in &intents {
            let bytes = i.encode();
            let (j, rest) = SignedIntent::decode(&bytes).unwrap();
            assert!(rest.is_empty());
            assert_eq!(&j, i);
        }
        let m = MergePayload { commitment: p };
        assert_eq!(MergePayload::decode(&m.encode()), Some(Some(m)));
    }

    #[test]
    fn the_id_binds_the_content_and_head_but_not_the_signature() {
        let keys = TestKeys::new(2, 2);
        let (p, _) = package(&keys, 3);
        let mut other = p.p;
        other.top = Frame(other.top.0 + 1);
        assert_ne!(other.id(), p.id());
        let mut other = p.p;
        other.head = Some(ChainHead {
            chain: ChainId(Hash([1; 32])),
            author: keys.key(0),
            seq: 1,
            entry: EntryRef(Hash([2; 32])),
        });
        assert_ne!(other.id(), p.id());
        // Signed again: another signature, the same id (a retry after a crash).
        let again = SignedCommitment { p: p.p, sig: Signature([7; 64]) };
        assert_eq!(again.id(), p.id());
        assert!(!again.verify(&keys));
    }

    #[test]
    fn a_package_that_differs_from_its_commitment_is_refused() {
        let keys = TestKeys::new(3, 2);
        let (p, intents) = package(&keys, 4);
        assert_eq!(verify_package(&p, &intents[..3], &keys), Err(PackageError::Count));
        let mut swapped = intents.clone();
        swapped.swap(1, 2);
        assert_eq!(verify_package(&p, &swapped, &keys), Err(PackageError::BadIntent(1)));
        let mut other = intents.clone();
        other[2] = PackageIntent { payload: Bytes::from_static(b"x"), ..other[2].intent.clone() }.sign(&keys.signer(0));
        assert_eq!(verify_package(&p, &other, &keys), Err(PackageError::Root));
        let mut forged = intents.clone();
        forged[0] = forged[0].intent.clone().sign(&keys.signer(1));
        assert_eq!(verify_package(&p, &forged, &keys), Err(PackageError::BadIntent(0)));
        let by_other = p.p.sign(&keys.signer(1));
        assert_eq!(verify_package(&by_other, &intents, &keys), Err(PackageError::BadCommitment));
    }

    #[test]
    fn decoding_never_panics_on_cut_or_flipped_bytes() {
        let keys = TestKeys::new(4, 2);
        let (p, intents) = package(&keys, 2);
        let m = MergePayload { commitment: p }.encode();
        for n in 0..m.len() {
            let _ = MergePayload::decode(&m[..n]);
            let mut f = m.to_vec();
            f[n] ^= 0x41;
            let _ = MergePayload::decode(&f);
        }
        let i = intents[0].encode();
        for n in 0..i.len() {
            assert!(SignedIntent::decode(&i[..n]).is_none());
        }
        assert_eq!(MergePayload::decode(b"pe.agree.merge/\x01"), Some(None));
        assert_eq!(MergePayload::decode(b"pe.agree.admit/"), None);
    }
}

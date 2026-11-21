//! F3's n = 2 branch in pe-world (world-model.md 2.3; @diverge-eng's design v5, L3, and): the branch
//! record a new line starts from, and the re-issued edits that keep their original's ids on it.
//!
//! A re-issued edit's payload is `REISSUE ‖ parent line ‖ chain ‖ seq u64 ‖ entry ‖ the original's payload bytes`,
//! the original's bytes unchanged. No transaction starts with `REISSUE` (its first byte is the actor's tag, 0 or 1),
//! so the two never read alike. A re-issue of a re-issued edit (a second branch) nests: its inner bytes are the
//! re-issued payload as it was, and its ids derive from the innermost reference.

use std::collections::BTreeMap;

use pe_core::{ChainId, EntryRef, Hash, LineId, MemberKey};

use crate::edit::Transaction;

/// The first byte of a re-issued edit's payload.
pub const REISSUE: u8 = 0xF0;

/// The bytes of one reference after [`REISSUE`]: line, chain, seq, entry.
const REF_LEN: usize = 32 + 32 + 8 + 32;

/// The bytes one re-issue adds in front of its original: [`REISSUE`] and the reference.
pub const REISSUE_PREFIX: usize = 1 + REF_LEN;

/// The deepest nesting a payload may have (one per branch the edit is carried across); past it, the payload is
/// malformed. [`crate::edit::MAX_EDIT_BYTES`] keeps this much room under `MAX_PAYLOAD`, so any edit can be re-issued
/// this many times (@diverge-eng's L1).
pub const MAX_REISSUE_DEPTH: usize = 8;

/// A re-issued edit's reference to its original: the entry at `seq` of `chain` on the parent `line`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reissue {
    pub line: LineId,
    pub chain: ChainId,
    pub seq: u64,
    pub entry: EntryRef,
}

/// One chain's head at the branch's frame, as `Branch` binds it: entries of `chain` above `head` are the suffix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tail {
    pub chain: ChainId,
    pub member: MemberKey,
    pub head: u64,
}

/// One original in the suffix that `Branch` seals: its place, its entry id, and [`payload_hash`] of its payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sealed {
    pub chain: ChainId,
    pub seq: u64,
    pub entry: EntryRef,
    pub payload: Hash,
}

/// Why [`crate::World::begin_branch`] refused a branch record. The world is unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BranchError {
    /// The parent is the world's own line: the replica switches the line first.
    SameLine,
    /// A chain is listed twice in the tails.
    DuplicateChain(ChainId),
    /// A sealed original is not above its chain's head, or its chain has no tail.
    NotInSuffix { chain: ChainId, seq: u64 },
    /// A sealed original is listed twice.
    DuplicateSealed { chain: ChainId, seq: u64 },
}

/// The hash `Branch` seals each original's payload with: `BLAKE3("pe.branch.payload" ‖ payload bytes)`, over the
/// original entry's payload exactly as signed (for a re-issued original, its whole re-issued payload).
pub fn payload_hash(payload: &[u8]) -> Hash {
    Hash::of("pe.branch.payload", payload)
}

/// The payload that re-issues `original` (the original entry's payload bytes, unchanged) under reference `r`.
pub fn reissue_payload(original: &[u8], r: &Reissue) -> Vec<u8> {
    let mut b = Vec::with_capacity(REISSUE_PREFIX + original.len());
    b.push(REISSUE);
    b.extend_from_slice(&r.line.0.0);
    b.extend_from_slice(&r.chain.0.0);
    b.extend_from_slice(&r.seq.to_le_bytes());
    b.extend_from_slice(&r.entry.0.0);
    b.extend_from_slice(original);
    b
}

/// The outermost reference of a re-issued payload, and the bytes it re-issues. `None` if `payload` is not a
/// re-issue, or is too short to hold a reference (then it is malformed as a transaction too).
pub fn split_reissue(payload: &[u8]) -> Option<(Reissue, &[u8])> {
    if payload.first() != Some(&REISSUE) || payload.len() < REISSUE_PREFIX {
        return None;
    }
    let h = |at: usize| Hash(payload[at..at + 32].try_into().unwrap());
    let r = Reissue {
        line: LineId(h(1)),
        chain: ChainId(h(33)),
        seq: u64::from_le_bytes(payload[65..73].try_into().unwrap()),
        entry: EntryRef(h(73)),
    };
    Some((r, &payload[REISSUE_PREFIX..]))
}

/// Where an edit's new ids, row tags and new asset ids derive from (2.1, 7.6, 7.9), and the entry its versions name
/// (7.6): the entry's own line, chain, seq and id, or, for a re-issued edit, its innermost original's. The author is
/// always the entry's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Origin {
    pub line: LineId,
    pub chain: ChainId,
    pub seq: u64,
    pub entry: EntryRef,
}

/// An edit entry's transaction and the origin its ids derive from, for a re-issued edit as for any other. `None` for
/// a payload that does not decode (7.6 step 1: `Malformed`). It checks no reference: [`crate::World::apply_edit`]
/// checks the outermost against the branch record, and the sealed hash binds the inner ones.
pub fn decode_edit(signed: &pe_seams::SignedEntry) -> Option<(Transaction, Origin)> {
    let entry = &signed.entry;
    let mut origin = Origin { line: entry.line, chain: entry.chain, seq: entry.seq, entry: signed.id() };
    let mut bytes: &[u8] = &entry.payload;
    for _ in 0..=MAX_REISSUE_DEPTH {
        match split_reissue(bytes) {
            None => return Transaction::decode(bytes).map(|tx| (tx, origin)),
            Some((r, inner)) => {
                origin = Origin { line: r.line, chain: r.chain, seq: r.seq, entry: r.entry };
                bytes = inner;
            }
        }
    }
    None
}

/// The branch record (world-model.md 6.6, the `lines` table): the line the world continues from, the chains' heads
/// there, and the sealed originals not re-issued yet. Empty until a branch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct BranchTable {
    pub(crate) parent: Option<LineId>,
    pub(crate) tails: BTreeMap<ChainId, (MemberKey, u64)>,
    pub(crate) sealed: BTreeMap<(ChainId, u64), (EntryRef, Hash)>,
}

/// The `lines` table's keys (6.6): 1 the branch's parent and tails, 2 ‖ chain ‖ seq (big-endian) a sealed original.
/// Key 0 is kept for the line list `Node.born_on` indexes (2.3), not built yet.
const KEY_BRANCH: u8 = 1;
const KEY_SEALED: u8 = 2;

impl BranchTable {
    /// The `lines` table's entries, in key order. None for a world that never branched.
    pub(crate) fn entries(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut e = Vec::new();
        let Some(parent) = self.parent else { return e };
        let mut v = parent.0.0.to_vec();
        v.extend_from_slice(&(self.tails.len() as u32).to_le_bytes());
        for (c, (m, head)) in &self.tails {
            v.extend_from_slice(&c.0.0);
            v.extend_from_slice(&m.0);
            v.extend_from_slice(&head.to_le_bytes());
        }
        e.push((vec![KEY_BRANCH], v));
        for ((c, seq), (entry, hash)) in &self.sealed {
            let mut k = vec![KEY_SEALED];
            k.extend_from_slice(&c.0.0);
            k.extend_from_slice(&seq.to_be_bytes());
            let mut v = entry.0.0.to_vec();
            v.extend_from_slice(&hash.0);
            e.push((k, v));
        }
        e
    }

    /// The record back from the `lines` table's entries. `None` if they do not decode, or are not one this world
    /// could hold (a sealed original outside the suffix, or sealed rows without the branch row).
    pub(crate) fn from_entries(entries: &[(&[u8], &[u8])]) -> Option<BranchTable> {
        let mut t = BranchTable::default();
        let h = |b: &[u8]| -> Option<Hash> { Some(Hash(b.try_into().ok()?)) };
        for (k, v) in entries {
            match *k.first()? {
                KEY_BRANCH if k.len() == 1 => {
                    let n = u32::from_le_bytes(v.get(32..36)?.try_into().ok()?) as usize;
                    if v.len() != 36 + n.checked_mul(72)? {
                        return None;
                    }
                    t.parent = Some(LineId(h(&v[..32])?));
                    for i in 0..n {
                        let r = &v[36 + 72 * i..36 + 72 * (i + 1)];
                        let chain = ChainId(h(&r[..32])?);
                        let member = MemberKey(r[32..64].try_into().ok()?);
                        let head = u64::from_le_bytes(r[64..72].try_into().ok()?);
                        if t.tails.insert(chain, (member, head)).is_some() {
                            return None;
                        }
                    }
                }
                KEY_SEALED if k.len() == 41 && v.len() == 64 => {
                    let chain = ChainId(h(&k[1..33])?);
                    let seq = u64::from_be_bytes(k[33..41].try_into().ok()?);
                    t.sealed.insert((chain, seq), (EntryRef(h(&v[..32])?), h(&v[32..])?));
                }
                _ => return None,
            }
        }
        let in_suffix = |&(c, seq): &(ChainId, u64)| t.tails.get(&c).is_some_and(|&(_, head)| seq > head);
        (t.sealed.keys().all(in_suffix) && (t.parent.is_some() || t.sealed.is_empty())).then_some(t)
    }

    /// Whether `author` may re-issue the original `r` names with payload `inner` now: the branch's parent, a chain
    /// in its tails whose member is `author`, a seq above that chain's head, and a sealed original not re-issued yet
    /// with this entry id and this payload hash.
    pub(crate) fn admits(&self, author: MemberKey, r: &Reissue, inner: &[u8]) -> bool {
        self.parent == Some(r.line)
            && self.tails.get(&r.chain).is_some_and(|&(m, head)| m == author && r.seq > head)
            && self.sealed.get(&(r.chain, r.seq)) == Some(&(r.entry, payload_hash(inner)))
    }
}

impl crate::World {
    /// Records the branch the world continues from (F3's n = 2 branch; @diverge-eng), replacing any
    /// earlier one: `parent` is the line it branched from, `tails` each chain's head at the branch's frame, and
    /// `sealed` the originals above those heads that may be re-issued, each once. Trusted: pe-replica calls it at the
    /// new line's genesis, after [`crate::World::switch_line`] and before any entry of the new line, from the
    /// agreed `Branch`. The record is the `lines` table (6.6), so the root moves with it. Refused, with the world
    /// unchanged, for a record no `Branch` can bind (see [`BranchError`]).
    pub fn begin_branch(&mut self, parent: LineId, tails: &[Tail], sealed: &[Sealed]) -> Result<(), BranchError> {
        if parent == self.line {
            return Err(BranchError::SameLine);
        }
        let mut t = BranchTable { parent: Some(parent), ..BranchTable::default() };
        for x in tails {
            if t.tails.insert(x.chain, (x.member, x.head)).is_some() {
                return Err(BranchError::DuplicateChain(x.chain));
            }
        }
        for s in sealed {
            if !t.tails.get(&s.chain).is_some_and(|&(_, head)| s.seq > head) {
                return Err(BranchError::NotInSuffix { chain: s.chain, seq: s.seq });
            }
            if t.sealed.insert((s.chain, s.seq), (s.entry, s.payload)).is_some() {
                return Err(BranchError::DuplicateSealed { chain: s.chain, seq: s.seq });
            }
        }
        self.branch = std::sync::Arc::new(t);
        Ok(())
    }

    /// The line the world branched from, if it did (the branch record's parent).
    pub fn branch_parent(&self) -> Option<LineId> {
        self.branch.parent
    }

    /// Whether the sealed original at (`chain`, `seq`) may still be re-issued.
    pub fn is_sealed(&self, chain: ChainId, seq: u64) -> bool {
        self.branch.sealed.contains_key(&(chain, seq))
    }
}

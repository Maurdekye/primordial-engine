//! pe-log (WS-agree, @world-chunks). The entry log: authors' chains, their validation in order, and the verdicts a
//! final cut gives the entries it covers.
//!
//! Specification: docs/spec/agreement.md (branch agree/spec). The rules follow the checked model, models/cuts
//! (`Cuts::segment`, `cut_ok` and `verdicts_at`):
//!
//! - A chain is keyed by (author, chain id). An entry signed by another key that claims a chain id lands in a chain of
//!   its own, which can never link to the real one's first entry.
//! - Every well-formed, correctly signed entry for this line is kept, forks included. Nothing takes effect here; a
//!   final cut decides (S3 v1.1: only an included entry can be void).
//! - A cut names, per chain, its head. [`ChainLog::segment`] walks from that head back along `prev` to the parent cut's
//!   head, and reports the first entry it lacks, so the member can fetch it by name before voting or finalizing.
//! - [`ChainLog::judge`] gives the verdicts on a final cut's newly covered segment: an entry stamped below the parent
//!   cut's frame is late; one stamped below any earlier stamp in its chain is back-stamped (an equal stamp is valid,
//!   ordered by seq); an entry the member holds at the same (chain, seq) as a covered one is a fork. Each is void.
//!   The rest take effect.

use std::collections::{BTreeMap, HashMap};

use pe_seams::{ChainId, EntryError, EntryRef, Frame, LineId, MemberKey, SignedEntry, Verifier};

/// A chain's identity: its author and its id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChainKey {
    pub author: MemberKey,
    pub chain: ChainId,
}

impl ChainKey {
    pub fn of(e: &SignedEntry) -> ChainKey {
        ChainKey { author: e.entry.author, chain: e.entry.chain }
    }
}

/// Why an entry is not kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rejected {
    /// It breaks the chain rules S1 checks alone.
    Shape(EntryError),
    /// It was made on another line.
    OtherLine,
    /// The signature is not its author's.
    BadSignature,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inserted {
    New,
    /// Already held: the same id.
    Duplicate,
}

/// Why a cut's segment can't be walked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SegmentError {
    /// An entry the walk needs is not held: fetch it by name and try again.
    Missing(EntryRef),
    /// The walk from the head does not reach the parent's head (it reached seq 1, or a seq that doesn't follow).
    NotLinked,
    /// The named head belongs to another chain than the one the cut names it for.
    WrongChain,
}

/// Why a proposal's segment gets no vote, beyond what [`SegmentError`] covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CutError {
    Segment(SegmentError),
    /// An entry is stamped at or beyond the cut's frame.
    BeyondFrame(EntryRef),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Effective,
    Void(VoidReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoidReason {
    /// Stamped below the parent cut's frame.
    Late,
    /// Stamped below an earlier entry's stamp in its chain.
    BackStamped,
    /// Another entry at the same (chain, seq) is the one a final cut covers.
    Fork,
}

#[derive(Default)]
struct Chain {
    by_id: HashMap<EntryRef, SignedEntry>,
    by_seq: BTreeMap<u64, Vec<EntryRef>>,
}

/// Every author's chains on one line.
pub struct ChainLog {
    line: LineId,
    chains: HashMap<ChainKey, Chain>,
}

/// [`ChainLog::judge`]'s answer: each entry's verdict, and the highest stamp through the head.
pub type Judged = (Vec<(EntryRef, Verdict)>, Option<Frame>);

impl ChainLog {
    pub fn new(line: LineId) -> ChainLog {
        ChainLog { line, chains: HashMap::new() }
    }

    pub fn line(&self) -> LineId {
        self.line
    }

    /// Keep a received entry if it is well formed, for this line and signed by its author.
    /// The acceptance checks without inserting: shape, line, signature (spec 2). The node runs them before it charges
    /// the author's rate budget, so a forged entry never spends an honest author's tokens.
    pub fn check(&self, e: &SignedEntry, verifier: &dyn Verifier) -> Result<(), Rejected> {
        e.entry.check_shape().map_err(Rejected::Shape)?;
        if e.entry.line != self.line {
            return Err(Rejected::OtherLine);
        }
        if !e.verify(verifier) {
            return Err(Rejected::BadSignature);
        }
        Ok(())
    }

    pub fn insert(&mut self, e: SignedEntry, verifier: &dyn Verifier) -> Result<Inserted, Rejected> {
        self.check(&e, verifier)?;
        let id = e.id();
        let chain = self.chains.entry(ChainKey::of(&e)).or_default();
        if chain.by_id.contains_key(&id) {
            return Ok(Inserted::Duplicate);
        }
        chain.by_seq.entry(e.entry.seq).or_default().push(id);
        chain.by_id.insert(id, e);
        Ok(Inserted::New)
    }

    /// Every chain held, sorted.
    pub fn keys(&self) -> Vec<ChainKey> {
        let mut k: Vec<ChainKey> = self.chains.keys().copied().collect();
        k.sort();
        k
    }

    /// The held entries that directly follow `after` in chain `key` (the chain's first entries when `after` is
    /// None): one normally, more at a fork, none at a hole or the chain's end. Sorted by id.
    pub fn next_of(&self, key: &ChainKey, after: Option<EntryRef>) -> Vec<&SignedEntry> {
        let Some(chain) = self.chains.get(key) else { return Vec::new() };
        let seq = match after {
            None => 1,
            Some(a) => match chain.by_id.get(&a) {
                Some(e) => e.entry.seq + 1,
                None => return Vec::new(),
            },
        };
        let mut out: Vec<&SignedEntry> = chain
            .by_seq
            .get(&seq)
            .into_iter()
            .flatten()
            .map(|id| &chain.by_id[id])
            .filter(|e| e.entry.prev == after)
            .collect();
        out.sort_by_key(|e| e.id());
        out
    }

    pub fn get(&self, key: &ChainKey, id: &EntryRef) -> Option<&SignedEntry> {
        self.chains.get(key)?.by_id.get(id)
    }

    /// The entries a cut adds to chain `key`: from `head` back to, not including, `from` (the parent cut's head, or
    /// None from the chain's start), in chain order.
    pub fn segment(
        &self,
        key: &ChainKey,
        from: Option<EntryRef>,
        head: EntryRef,
    ) -> Result<Vec<EntryRef>, SegmentError> {
        let chain = self.chains.get(key).ok_or(SegmentError::Missing(head))?;
        let mut seg = Vec::new();
        let mut x = head;
        let mut want_seq: Option<u64> = None;
        loop {
            if Some(x) == from {
                break;
            }
            let e = chain.by_id.get(&x).ok_or(SegmentError::Missing(x))?;
            if ChainKey::of(e) != *key {
                return Err(SegmentError::WrongChain);
            }
            if want_seq.is_some_and(|s| s != e.entry.seq) {
                return Err(SegmentError::NotLinked);
            }
            seg.push(x);
            match e.entry.prev {
                Some(p) => {
                    want_seq = Some(e.entry.seq - 1);
                    x = p;
                }
                None if from.is_none() => break,
                None => return Err(SegmentError::NotLinked),
            }
        }
        seg.reverse();
        Ok(seg)
    }

    /// The vote check on one chain of a proposal: the segment walks, and every entry in it is stamped below the
    /// cut's frame. Signatures and the line were checked at insert.
    pub fn check_cut(
        &self,
        key: &ChainKey,
        from: Option<EntryRef>,
        head: EntryRef,
        frame: Frame,
    ) -> Result<Vec<EntryRef>, CutError> {
        let seg = self.segment(key, from, head).map_err(CutError::Segment)?;
        for x in &seg {
            if self.chains[key].by_id[x].entry.stamp >= frame {
                return Err(CutError::BeyondFrame(*x));
            }
        }
        Ok(seg)
    }

    /// The verdicts a final cut gives on chain `key`: on each entry it newly covers (`from` back from `head`), and on
    /// every held fork of one. `parent_frame` is the parent cut's frame. `high_before` is the highest stamp on the chain
    /// through `from` (void entries included), which the caller keeps per chain, so only the new segment is walked: a
    /// stamp below it is back-stamped. Returns the verdicts and the highest stamp through `head`.
    pub fn judge(
        &self,
        key: &ChainKey,
        from: Option<EntryRef>,
        head: EntryRef,
        parent_frame: Frame,
        high_before: Option<Frame>,
    ) -> Result<Judged, SegmentError> {
        let seg = self.segment(key, from, head)?;
        let chain = &self.chains[key];
        let mut high = high_before;
        let mut out = Vec::new();
        for x in seg {
            let e = &chain.by_id[&x].entry;
            let v = if e.stamp < parent_frame {
                Verdict::Void(VoidReason::Late)
            } else if high.is_some_and(|h| e.stamp < h) {
                Verdict::Void(VoidReason::BackStamped)
            } else {
                Verdict::Effective
            };
            high = Some(high.map_or(e.stamp, |h| h.max(e.stamp)));
            out.push((x, v));
            for f in chain.by_seq.get(&e.seq).into_iter().flatten() {
                if *f != x {
                    out.push((*f, Verdict::Void(VoidReason::Fork)));
                }
            }
        }
        Ok((out, high))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use pe_seams::Class;
    use pe_seams::test_util::{EntryBuilder, TestKeys};

    fn setup() -> (TestKeys, EntryBuilder, ChainLog) {
        let keys = TestKeys::new(7, 3);
        let b = EntryBuilder::new(&keys, 1);
        (keys, b, ChainLog::new(LineId::origin()))
    }

    fn next(b: &mut EntryBuilder, stamp: u64) -> SignedEntry {
        b.next(Class::Input, Frame(stamp), Bytes::from_static(b"x"))
    }

    #[test]
    fn segment_walks_in_order_and_reports_a_hole() {
        let (keys, mut b, mut log) = setup();
        let (e1, e2, e3) = (next(&mut b, 1), next(&mut b, 2), next(&mut b, 3));
        let key = ChainKey::of(&e1);
        log.insert(e1.clone(), &keys).unwrap();
        log.insert(e3.clone(), &keys).unwrap();
        assert_eq!(log.segment(&key, None, e3.id()), Err(SegmentError::Missing(e2.id())));
        log.insert(e2.clone(), &keys).unwrap();
        assert_eq!(log.segment(&key, None, e3.id()), Ok(vec![e1.id(), e2.id(), e3.id()]));
        assert_eq!(log.segment(&key, Some(e1.id()), e3.id()), Ok(vec![e2.id(), e3.id()]));
        assert_eq!(log.insert(e2, &keys), Ok(Inserted::Duplicate));
    }

    #[test]
    fn insert_rejects_another_line_and_a_bad_signature() {
        let (keys, mut b, mut log) = setup();
        let e1 = next(&mut b, 1);
        let mut other = ChainLog::new(LineId(pe_seams::Hash([9; 32])));
        assert_eq!(other.insert(e1.clone(), &keys), Err(Rejected::OtherLine));
        let mut forged = e1.clone();
        forged.sig.0[0] ^= 1;
        assert_eq!(log.insert(forged, &keys), Err(Rejected::BadSignature));
    }

    #[test]
    fn a_forged_chain_claim_never_links() {
        // Member 2 signs an entry claiming member 1's chain id: it lands in (member 2, that id) and can't walk.
        let (keys, mut b1, mut log) = setup();
        let e1 = next(&mut b1, 1);
        log.insert(e1.clone(), &keys).unwrap();
        let mut b2 = EntryBuilder::new(&keys, 2);
        let mut fake = b2.next_unsigned(Class::Input, Frame(2), Bytes::new());
        fake = pe_seams::Entry { chain: e1.entry.chain, seq: 2, prev: Some(e1.id()), nonce: None, ..fake };
        let fake = b2.sign(fake);
        log.insert(fake.clone(), &keys).unwrap();
        let real = ChainKey::of(&e1);
        assert_eq!(log.segment(&real, None, fake.id()), Err(SegmentError::Missing(fake.id())));
        assert_eq!(log.segment(&ChainKey::of(&fake), None, fake.id()), Err(SegmentError::Missing(e1.id())));
    }

    #[test]
    fn check_cut_refuses_an_entry_at_the_frame() {
        let (keys, mut b, mut log) = setup();
        let (e1, e2) = (next(&mut b, 1), next(&mut b, 3));
        let key = ChainKey::of(&e1);
        log.insert(e1.clone(), &keys).unwrap();
        log.insert(e2.clone(), &keys).unwrap();
        assert_eq!(log.check_cut(&key, None, e2.id(), Frame(3)), Err(CutError::BeyondFrame(e2.id())));
        assert!(log.check_cut(&key, None, e2.id(), Frame(4)).is_ok());
    }

    #[test]
    fn judge_voids_late_back_stamped_and_forks() {
        let (keys, mut b, mut log) = setup();
        // seq 1 at 5, seq 2 back-stamped at 4, seq 3 at 5 (equal to the earlier high: valid).
        let e1 = next(&mut b, 5);
        let e2 = next(&mut b, 4);
        let e3 = next(&mut b, 5);
        let key = ChainKey::of(&e1);
        for e in [&e1, &e2, &e3] {
            log.insert(e.clone(), &keys).unwrap();
        }
        let v = log.judge(&key, None, e3.id(), Frame(0), None).unwrap().0;
        assert_eq!(
            v,
            vec![
                (e1.id(), Verdict::Effective),
                (e2.id(), Verdict::Void(VoidReason::BackStamped)),
                (e3.id(), Verdict::Effective)
            ]
        );
        // Covered after a cut whose frame is 6: all three are late.
        let v = log.judge(&key, None, e3.id(), Frame(6), None).unwrap().0;
        assert!(v.iter().all(|(_, x)| *x == Verdict::Void(VoidReason::Late)));
        // A fork of seq 3 that the member holds is void once seq 3 is covered.
        let mut fork = e3.entry.clone();
        fork.payload = Bytes::from_static(b"y");
        let fork = b.sign(fork);
        log.insert(fork.clone(), &keys).unwrap();
        let high = log.judge(&key, None, e2.id(), Frame(0), None).unwrap().1;
        let v = log.judge(&key, Some(e2.id()), e3.id(), Frame(0), high).unwrap().0;
        assert_eq!(v, vec![(e3.id(), Verdict::Effective), (fork.id(), Verdict::Void(VoidReason::Fork))]);
    }

    #[test]
    fn judge_needs_the_earlier_chain() {
        // The d1 case: an entry past a hole is never judged alone.
        let (keys, mut b, mut log) = setup();
        let (e1, e2) = (next(&mut b, 2), next(&mut b, 1));
        let key = ChainKey::of(&e1);
        log.insert(e2.clone(), &keys).unwrap();
        assert_eq!(log.judge(&key, None, e2.id(), Frame(0), None), Err(SegmentError::Missing(e1.id())));
        log.insert(e1, &keys).unwrap();
        assert_eq!(
            log.judge(&key, None, e2.id(), Frame(0), None).unwrap().0[1].1,
            Verdict::Void(VoidReason::BackStamped)
        );
    }
}

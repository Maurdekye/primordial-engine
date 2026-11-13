//! Building a cut as the leader, and checking a proposed one before voting (docs/spec/agreement.md 3; the model's
//! `build_cut` and `cut_ok`).
//!
//! A cut names, per chain, its head: the last entry it includes. A child cut keeps every chain of its parent, each
//! head equal to the parent's or further along the same chain; a chain new to the cut starts at its seq 1, which
//! binds the chain id to its author (`Entry::check_shape`). Every entry it newly includes is held, linked in chain
//! order back to the parent's head, and stamped below the cut's frame. The frame is above the parent's and at most the
//! voter's present plus a tolerance: never a bound relative to the parent (@design-reviewer).

use pe_log::{ChainKey, ChainLog, CutError, SegmentError};
use pe_seams::{ChainHead, Cut, CutRef, EntryRef, Frame};

/// Why a member does not vote for a proposed cut.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reject {
    /// Not above the parent's frame.
    FrameNotAbove,
    /// Beyond the voter's present plus the tolerance: it would void honest entries not yet sent as late.
    FrameInFuture,
    /// Heads not sorted by chain, or a chain twice.
    Unsorted,
    /// A chain of the parent is missing.
    DroppedChain,
    /// A head is not on its chain after the parent's head (a fork of it, another chain, or behind it).
    NotLinked(ChainHead),
    /// An entry it includes is stamped at or beyond its frame.
    BeyondFrame(EntryRef),
    /// The cut is not the parent's child (line or height).
    WrongPlace,
}

/// The outcome of a check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Check {
    Ok,
    /// Entries this member lacks: fetch them by name, then check again.
    Missing(Vec<EntryRef>),
    Reject(Reject),
}

fn key(h: &ChainHead) -> ChainKey {
    ChainKey { author: h.author, chain: h.chain }
}

/// The leader's cut at `frame`, the child of `parent`: for every chain held, the parent's head extended along the
/// chain while the next entry is held, unforked and stamped below `frame`. A chain stops at a hole or a fork.
pub fn build(log: &ChainLog, parent: &Cut, frame: Frame) -> Cut {
    let mut heads: Vec<ChainHead> = Vec::new();
    for k in log.keys() {
        let from = parent.heads.binary_search_by(|h| h.chain.cmp(&k.chain)).ok().map(|i| parent.heads[i]);
        if from.is_some_and(|f| f.author != k.author) {
            // Another key claiming this chain id: it can't link to the real chain's start.
            continue;
        }
        let mut head = from;
        loop {
            let next = log.next_of(&k, head.map(|h| h.entry));
            if next.len() != 1 || next[0].entry.stamp >= frame {
                break;
            }
            let e = next[0];
            head = Some(ChainHead { chain: k.chain, author: k.author, seq: e.entry.seq, entry: e.id() });
        }
        if let Some(h) = head {
            heads.push(h);
        }
    }
    heads.sort();
    heads.dedup_by(|a, b| a.chain == b.chain);
    Cut { at: CutRef { line: parent.at.line, height: parent.at.height + 1, frame }, heads }
}

/// Check `cut`, proposed as the child of `parent`, as a member whose present frame is `present`.
pub fn check(log: &ChainLog, parent: &Cut, cut: &Cut, present: Frame, tolerance: u64) -> Check {
    if cut.at.line != parent.at.line || cut.at.height != parent.at.height + 1 {
        return Check::Reject(Reject::WrongPlace);
    }
    if cut.at.frame <= parent.at.frame {
        return Check::Reject(Reject::FrameNotAbove);
    }
    if cut.at.frame.0 > present.0.saturating_add(tolerance) {
        return Check::Reject(Reject::FrameInFuture);
    }
    if cut.heads.windows(2).any(|w| w[0].chain >= w[1].chain) {
        return Check::Reject(Reject::Unsorted);
    }
    let mut missing = Vec::new();
    let mut pi = 0;
    for h in &cut.heads {
        if pi < parent.heads.len() && parent.heads[pi].chain < h.chain {
            return Check::Reject(Reject::DroppedChain);
        }
        let from = if pi < parent.heads.len() && parent.heads[pi].chain == h.chain {
            let f = parent.heads[pi];
            pi += 1;
            if f.author != h.author || h.seq < f.seq {
                return Check::Reject(Reject::NotLinked(*h));
            }
            if f.seq == h.seq {
                if f.entry != h.entry {
                    return Check::Reject(Reject::NotLinked(*h));
                }
                continue;
            }
            Some(f.entry)
        } else {
            None
        };
        match log.check_cut(&key(h), from, h.entry, cut.at.frame) {
            Ok(seg) => {
                // The head's seq is the one the cut names.
                let last = seg.last().copied();
                if last != Some(h.entry) || log.get(&key(h), &h.entry).map(|e| e.entry.seq) != Some(h.seq) {
                    return Check::Reject(Reject::NotLinked(*h));
                }
            }
            Err(CutError::Segment(SegmentError::Missing(x))) => missing.push(x),
            Err(CutError::Segment(_)) => return Check::Reject(Reject::NotLinked(*h)),
            Err(CutError::BeyondFrame(x)) => return Check::Reject(Reject::BeyondFrame(x)),
        }
    }
    if pi < parent.heads.len() {
        return Check::Reject(Reject::DroppedChain);
    }
    if missing.is_empty() { Check::Ok } else { Check::Missing(missing) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use pe_seams::test_util::{EntryBuilder, TestKeys};
    use pe_seams::{Class, LineId, SignedEntry};

    fn e(b: &mut EntryBuilder, stamp: u64) -> SignedEntry {
        b.next(Class::Input, Frame(stamp), Bytes::from_static(b"x"))
    }

    fn genesis() -> Cut {
        Cut { at: CutRef { line: LineId::origin(), height: 0, frame: Frame(0) }, heads: Vec::new() }
    }

    struct Fx {
        keys: TestKeys,
        a: EntryBuilder,
        b: EntryBuilder,
        log: ChainLog,
    }

    fn fx() -> Fx {
        let keys = TestKeys::new(3, 4);
        let a = EntryBuilder::new(&keys, 0);
        let b = EntryBuilder::new(&keys, 1);
        Fx { keys, a, b, log: ChainLog::new(LineId::origin()) }
    }

    #[test]
    fn the_leader_includes_linked_entries_below_the_frame() {
        let mut f = fx();
        let (a1, a2, a3) = (e(&mut f.a, 1), e(&mut f.a, 4), e(&mut f.a, 9));
        let (b1, b2) = (e(&mut f.b, 2), e(&mut f.b, 3));
        for x in [&a1, &a2, &a3, &b2] {
            f.log.insert(x.clone(), &f.keys).unwrap();
        }
        let c1 = build(&f.log, &genesis(), Frame(6));
        // a stops before a3 (stamp 9 >= 6); b has a hole at b1.
        assert_eq!(c1.heads.len(), 1);
        assert_eq!(c1.heads[0].entry, a2.id());
        assert_eq!(check(&f.log, &genesis(), &c1, Frame(6), 0), Check::Ok);
        f.log.insert(b1.clone(), &f.keys).unwrap();
        let c2 = build(&f.log, &c1, Frame(12));
        assert_eq!(c2.heads.len(), 2);
        assert!(c2.includes(&f.a.chain(), 3) && c2.includes(&f.b.chain(), 2));
        assert_eq!(check(&f.log, &c1, &c2, Frame(12), 0), Check::Ok);
    }

    #[test]
    fn a_voter_fetches_what_it_lacks() {
        let mut f = fx();
        let (a1, a2) = (e(&mut f.a, 1), e(&mut f.a, 2));
        f.log.insert(a1.clone(), &f.keys).unwrap();
        f.log.insert(a2.clone(), &f.keys).unwrap();
        let c = build(&f.log, &genesis(), Frame(5));
        let mut voter = ChainLog::new(LineId::origin());
        voter.insert(a2.clone(), &f.keys).unwrap();
        assert_eq!(check(&voter, &genesis(), &c, Frame(5), 0), Check::Missing(vec![a1.id()]));
        voter.insert(a1, &f.keys).unwrap();
        assert_eq!(check(&voter, &genesis(), &c, Frame(5), 0), Check::Ok);
    }

    #[test]
    fn frames_rise_and_never_pass_the_voters_present_plus_tolerance() {
        let f = fx();
        let c = build(&f.log, &genesis(), Frame(10));
        assert_eq!(check(&f.log, &genesis(), &c, Frame(8), 2), Check::Ok);
        // The future-frame proposal (spec 3; harness test): beyond present + tolerance gets no honest vote.
        assert_eq!(check(&f.log, &genesis(), &c, Frame(7), 2), Check::Reject(Reject::FrameInFuture));
        let same = Cut { at: CutRef { frame: Frame(10), height: 2, ..c.at }, heads: Vec::new() };
        assert_eq!(check(&f.log, &c, &same, Frame(20), 0), Check::Reject(Reject::FrameNotAbove));
    }

    #[test]
    fn a_child_keeps_every_chain_and_moves_heads_only_forward() {
        let mut f = fx();
        let (a1, a2) = (e(&mut f.a, 1), e(&mut f.a, 2));
        for x in [&a1, &a2] {
            f.log.insert(x.clone(), &f.keys).unwrap();
        }
        let c1 = build(&f.log, &genesis(), Frame(5));
        let dropped = Cut { at: CutRef { height: 2, frame: Frame(9), ..c1.at }, heads: Vec::new() };
        assert_eq!(check(&f.log, &c1, &dropped, Frame(9), 0), Check::Reject(Reject::DroppedChain));
        let mut back = c1.clone();
        back.at = CutRef { height: 2, frame: Frame(9), ..c1.at };
        back.heads[0] = ChainHead { seq: 1, entry: a1.id(), ..back.heads[0] };
        assert!(matches!(check(&f.log, &c1, &back, Frame(9), 0), Check::Reject(Reject::NotLinked(_))));
    }

    #[test]
    fn a_head_stamped_at_the_frame_or_a_wrong_seq_is_refused() {
        let mut f = fx();
        let a1 = e(&mut f.a, 5);
        f.log.insert(a1.clone(), &f.keys).unwrap();
        let k = ChainKey::of(&a1);
        let at = CutRef { line: LineId::origin(), height: 1, frame: Frame(5) };
        let head = ChainHead { chain: k.chain, author: k.author, seq: 1, entry: a1.id() };
        let c = Cut { at, heads: vec![head] };
        assert_eq!(check(&f.log, &genesis(), &c, Frame(5), 0), Check::Reject(Reject::BeyondFrame(a1.id())));
        let c = Cut { at: CutRef { frame: Frame(6), ..at }, heads: vec![ChainHead { seq: 4, ..head }] };
        assert!(matches!(check(&f.log, &genesis(), &c, Frame(6), 0), Check::Reject(Reject::NotLinked(_))));
    }

    #[test]
    fn a_chain_claimed_by_another_key_never_links() {
        let mut f = fx();
        let a1 = e(&mut f.a, 1);
        f.log.insert(a1.clone(), &f.keys).unwrap();
        // Member 1 signs an entry claiming member 0's chain id at seq 1: its shape check fails (the chain id is
        // derived from the author), so it is never even held; a later seq claiming it can't walk to a start.
        let mut forged = f.b.next_unsigned(Class::Input, Frame(2), Bytes::from_static(b"f"));
        forged.chain = f.a.chain();
        assert!(f.log.insert(f.b.sign(forged), &f.keys).is_err());
        let c = build(&f.log, &genesis(), Frame(5));
        assert_eq!(c.heads.len(), 1);
        assert_eq!(c.heads[0].author, f.keys.key(0));
    }
}

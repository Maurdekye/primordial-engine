//! The reference member for a root split with no quorum root (divergence.md 5, step 3; @coordinator-opus's L4'
//! rulings m9cef15c38b88 and m012a33b972f6, with @fable-reviewer's rule me87d5259485b).
//!
//! Every member must name the same reference from the same evidence, whenever its own evidence arrives. So the choice
//! is a pure function of what every member ends up holding identically:
//! - the newest final cut (nothing becomes final during an episode, and a member behind on finality waits);
//! - each voter's self-test verdict for this split (final for the split: a second, different one is equivocation,
//!   which agreement shows as `Failed`, S3 v1.3);
//! - the latch: a member any of whose verdicts for this split showed `Failed` is out for the whole episode.

use std::collections::BTreeSet;

use pe_seams::{CutRef, Hash, Hasher, MemberKey, SelfTest};

/// The tie-break hash of step 3: `H(the newest final cut's line, height and frame ‖ member key)`. Every voter holds
/// the same newest final cut once it has caught up, and it stays fixed for the episode.
pub fn tie_hash(final_cut: Option<&CutRef>, member: &MemberKey) -> Hash {
    let mut h = Hasher::new("pe.diverge.split");
    match final_cut {
        Some(c) => h.write(&[1]).write_hash(&c.line.0).write_u64(c.height).write_u64(c.frame.0),
        None => h.write(&[0]),
    };
    h.write(&member.0);
    h.finish()
}

/// What a member may do with a split's standings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pick {
    /// Not yet: this member is behind on finality, or a voter's verdict for this split has not arrived.
    Wait(&'static str),
    /// The reference; `None` if every voter is latched out.
    Ready(Option<MemberKey>),
}

/// Add every voter whose verdict for `split` is `Failed` to the latch.
pub fn latch(split: &CutRef, roots: &[(Hash, Vec<(MemberKey, SelfTest)>)], failed: &mut BTreeSet<MemberKey>) {
    for (_, voters) in roots {
        for (m, t) in voters {
            if *t == (SelfTest::Failed { split: *split }) {
                failed.insert(*m);
            }
        }
    }
}

/// Choose the reference (step 3). `final_now` is the newest final cut whose entries this member holds
/// (`EntrySource::final_cut`); `final_seen` the newest final cut it has heard of. Wait until they are the same cut and
/// every voter's verdict names this split; then the reference is the voter not latched out with the smallest
/// [`tie_hash`]. "Most voters hold" is not used in M1: it matters from n = 4 (a 2-1-1 split), and comes back with a
/// model check. At n = 2 with both verdicts `Passed`, the pick is arbitrary, and the same on both members.
pub fn pick(
    split: &CutRef,
    final_now: Option<&CutRef>,
    final_seen: Option<&CutRef>,
    roots: &[(Hash, Vec<(MemberKey, SelfTest)>)],
    failed: &BTreeSet<MemberKey>,
) -> Pick {
    if final_now != final_seen {
        return Pick::Wait("behind on finality");
    }
    let voters: Vec<&(MemberKey, SelfTest)> = roots.iter().flat_map(|r| r.1.iter()).collect();
    if voters.iter().any(|(_, t)| t.split() != Some(*split)) {
        return Pick::Wait("a voter's verdict for this split has not arrived");
    }
    let reference = voters
        .iter()
        .filter(|(m, t)| !failed.contains(m) && !matches!(t, SelfTest::Failed { .. }))
        .map(|(m, _)| (tie_hash(final_now, m), *m))
        .min()
        .map(|x| x.1);
    Pick::Ready(reference)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pe_seams::{Frame, LineId};

    fn split() -> CutRef {
        CutRef { line: LineId(Hash([3; 32])), height: 7, frame: Frame(40) }
    }
    fn fin() -> CutRef {
        CutRef { line: LineId(Hash([3; 32])), height: 6, frame: Frame(32) }
    }
    fn k(n: u8) -> MemberKey {
        MemberKey([n; 32])
    }
    fn passed() -> SelfTest {
        SelfTest::Passed { split: split() }
    }
    fn failed() -> SelfTest {
        SelfTest::Failed { split: split() }
    }
    fn roots(v: &[(u8, u8, SelfTest)]) -> Vec<(Hash, Vec<(MemberKey, SelfTest)>)> {
        let mut out: Vec<(Hash, Vec<(MemberKey, SelfTest)>)> = Vec::new();
        for (m, r, t) in v {
            match out.iter_mut().find(|x| x.0 == Hash([*r; 32])) {
                Some(x) => x.1.push((k(*m), *t)),
                None => out.push((Hash([*r; 32]), vec![(k(*m), *t)])),
            }
        }
        out
    }
    fn go(r: &[(Hash, Vec<(MemberKey, SelfTest)>)], latched: &BTreeSet<MemberKey>) -> Pick {
        pick(&split(), Some(&fin()), Some(&fin()), r, latched)
    }

    #[test]
    fn a_failed_self_test_is_never_the_reference() {
        let r = roots(&[(1, 9, failed()), (2, 7, passed())]);
        assert_eq!(go(&r, &BTreeSet::new()), Pick::Ready(Some(k(2))));
        assert_eq!(go(&roots(&[(1, 9, failed())]), &BTreeSet::new()), Pick::Ready(None));
    }

    #[test]
    fn the_pick_waits_for_every_voters_verdict_for_this_split() {
        // A's view before B's failed verdict arrives (@design-reviewer's n = 2 race): B still shows NotRun, or a
        // verdict from an earlier split. A must not pick.
        let earlier = CutRef { height: 3, ..split() };
        for b in [SelfTest::NotRun, SelfTest::Passed { split: earlier }, SelfTest::Failed { split: earlier }] {
            let r = roots(&[(1, 9, passed()), (2, 7, b)]);
            assert!(matches!(go(&r, &BTreeSet::new()), Pick::Wait(_)), "{b:?}");
        }
    }

    #[test]
    fn a_member_behind_on_finality_waits() {
        let r = roots(&[(1, 9, passed()), (2, 7, passed())]);
        let older = CutRef { height: 5, ..fin() };
        assert!(matches!(pick(&split(), Some(&older), Some(&fin()), &r, &BTreeSet::new()), Pick::Wait(_)));
        assert!(matches!(pick(&split(), None, Some(&fin()), &r, &BTreeSet::new()), Pick::Wait(_)));
    }

    #[test]
    fn equivocated_verdicts_in_either_order_give_both_observers_the_same_reference() {
        // B sends Passed then Failed for one split. One observer sees them in that order, the other the reverse;
        // agreement shows Failed once it has both, and the latch keeps it. Both pick A.
        for order in [[passed(), failed()], [failed(), passed()]] {
            let mut latched = BTreeSet::new();
            let mut last = None;
            for b in order {
                let r = roots(&[(1, 9, passed()), (2, 7, b)]);
                latch(&split(), &r, &mut latched);
                last = Some(go(&r, &latched));
            }
            assert_eq!(last, Some(Pick::Ready(Some(k(1)))), "{order:?}");
        }
    }

    #[test]
    fn the_answer_does_not_depend_on_order() {
        let s = roots(&[(1, 9, passed()), (2, 8, passed()), (3, 8, passed())]);
        let mut r = s.clone();
        r.reverse();
        for x in &mut r {
            x.1.reverse();
        }
        assert_eq!(go(&s, &BTreeSet::new()), go(&r, &BTreeSet::new()));
    }

    #[test]
    fn a_two_member_tie_goes_by_the_hash_of_the_final_cut_and_key() {
        let r = roots(&[(1, 9, passed()), (2, 8, passed())]);
        let Pick::Ready(Some(a)) = go(&r, &BTreeSet::new()) else { panic!() };
        let b = if a == k(1) { k(2) } else { k(1) };
        assert!(tie_hash(Some(&fin()), &a) < tie_hash(Some(&fin()), &b));
    }
}

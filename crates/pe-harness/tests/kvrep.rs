//! KvSeamReplica against S2 v1's contract, driven by a hand-held entry source.

use std::collections::BTreeMap;

use pe_harness::kv::{Op, Template};
use pe_harness::kvrep::{KvSeamReplica, payload};
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{
    ChainHead, Class, Cut, CutRef, EntrySource, FinalCut, Finalized, Frame, LineId, Outcome, Reason, Replica,
    RootError, SignedEntry,
};

/// Entries by frame, a changed-from mark, a final cut and a trim (S3 v1.2), set by the test.
#[derive(Default)]
struct Src {
    at: BTreeMap<u64, Vec<SignedEntry>>,
    changed: Option<Frame>,
    fin: Option<FinalCut>,
    /// Frames below it are no longer kept.
    kept: u64,
    /// The last `keep_final_from(cv, floor)` the replica sent.
    asked: Option<(Frame, Frame)>,
}

impl Src {
    fn put(&mut self, e: SignedEntry) {
        let f = e.entry.stamp.0;
        self.at.entry(f).or_default().push(e);
        self.changed = Some(Frame(self.changed.map_or(f, |c| c.0.min(f))));
    }
}

impl EntrySource for Src {
    fn final_cut(&self) -> Option<&FinalCut> {
        self.fin.as_ref()
    }
    fn entries_at(&self, frame: Frame) -> Option<&[SignedEntry]> {
        if frame.0 < self.kept {
            return None;
        }
        Some(self.at.get(&frame.0).map_or(&[], |v| v.as_slice()))
    }
    fn kept_from(&self) -> Frame {
        Frame(self.kept)
    }
    fn keep_final_from(&mut self, cv: Frame, floor: Frame) {
        self.asked = Some((cv, floor));
    }
    fn take_changed_from(&mut self) -> Option<Frame> {
        self.changed.take()
    }
}

fn setup() -> (TestKeys, KvSeamReplica, Src) {
    let keys = TestKeys::new(1, 3);
    let r = KvSeamReplica::new(keys.sorted_keys(), &Template::AllAdmin, &[10]);
    (keys, r, Src::default())
}

fn set(v: i64) -> bytes::Bytes {
    payload(&[Op::Set { obj: 10, field: 0, value: v }])
}

fn cut_over(frame: u64, entries: &[&SignedEntry]) -> Cut {
    let mut heads: Vec<ChainHead> = entries
        .iter()
        .map(|e| ChainHead { chain: e.entry.chain, author: e.entry.author, seq: e.entry.seq, entry: e.id() })
        .collect();
    heads.sort_by_key(|h| h.chain);
    Cut { at: CutRef { line: LineId::origin(), height: 1, frame: Frame(frame) }, heads }
}

#[test]
fn advance_applies_entries_and_records_outcomes() {
    let (keys, mut r, mut src) = setup();
    let mut a = EntryBuilder::new(&keys, 0);
    let e = a.next(Class::Edit, Frame(3), set(7));
    src.put(e.clone());
    let adv = r.advance(Frame(10), &mut src);
    assert_eq!((adv.stepped, adv.rolled_back_to), (10, None));
    assert_eq!(r.frame(), Frame(10));
    assert_eq!(r.state().field(10, 0), Some(7));
    assert!(matches!(r.outcome(&e.id()), Some((Frame(3), Outcome::Applied { .. }))));
}

#[test]
fn a_late_entry_rolls_back_and_ends_where_an_on_time_one_would() {
    let (keys, mut late, mut src) = setup();
    let mut a = EntryBuilder::new(&keys, 0);
    let mut b = EntryBuilder::new(&keys, 1);
    let e1 = a.next(Class::Edit, Frame(2), set(1));
    let e2 = b.next(Class::Edit, Frame(5), payload(&[Op::Add { obj: 10, field: 0, delta: 10 }]));
    src.put(e1.clone());
    late.advance(Frame(20), &mut src);
    src.put(e2.clone()); // arrives late, stamped at 5
    let adv = late.advance(Frame(20), &mut src);
    assert_eq!(adv.rolled_back_to, Some(Frame(5)));
    assert_eq!(adv.resimulated, 15);

    let (_, mut on_time, mut src2) = setup();
    src2.put(e1);
    src2.put(e2);
    on_time.advance(Frame(20), &mut src2);
    assert_eq!(late.state().root_hash(), on_time.state().root_hash());
    assert_eq!(late.state().field(10, 0), Some(11));
}

#[test]
fn root_at_cut_uses_exactly_the_cut_and_leaves_the_live_state_alone() {
    let (keys, mut r, mut src) = setup();
    let mut a = EntryBuilder::new(&keys, 0);
    let mut b = EntryBuilder::new(&keys, 1);
    let ea = a.next(Class::Edit, Frame(2), set(1));
    let eb = b.next(Class::Edit, Frame(4), payload(&[Op::Add { obj: 10, field: 0, delta: 5 }]));
    src.put(ea.clone());
    src.put(eb.clone());
    r.advance(Frame(12), &mut src);
    let live = r.state().root_hash();
    let only_a = r.root_at_cut(&cut_over(10, &[&ea]), &mut src).unwrap();
    let both = r.root_at_cut(&cut_over(10, &[&ea, &eb]), &mut src).unwrap();
    assert_ne!(only_a, both, "the cut without b's entry has another root");
    assert_eq!(r.state().root_hash(), live, "the live state is unchanged");
    assert_eq!(r.root_at_cut(&cut_over(30, &[&ea]), &mut src), Err(RootError::NotYetSimulated));
}

#[test]
fn finalize_moves_the_floor_and_rollbacks_stop_there() {
    let (keys, mut r, mut src) = setup();
    let mut a = EntryBuilder::new(&keys, 0);
    let ea = a.next(Class::Edit, Frame(2), set(1));
    src.put(ea.clone());
    r.advance(Frame(15), &mut src);
    let cut = cut_over(10, &[&ea]);
    let root = r.root_at_cut(&cut, &mut src).unwrap();
    assert_eq!(r.finalize(&FinalCut { cut: cut.clone(), root, voters: vec![] }, &mut src), Finalized::Agreed);
    // S3 v1.2's "who asks": the replica tells the source its new Cv and floor from `finalize`.
    assert_eq!(src.asked, Some((Frame(10), Frame(10))), "finalize did not call keep_final_from");
    // An entry stamped below the floor changes nothing below it: the rollback stops at the floor.
    let mut b = EntryBuilder::new(&keys, 1);
    src.put(b.next(Class::Edit, Frame(4), set(99)));
    let adv = r.advance(Frame(15), &mut src);
    assert!(adv.rolled_back_to.is_none_or(|f| f >= Frame(10)), "{adv:?}");
    assert_eq!(r.root_at_cut(&cut_over(5, &[&ea]), &mut src), Err(RootError::NotKept));
}

#[test]
fn finalize_with_another_root_is_a_divergence() {
    let (keys, mut r, mut src) = setup();
    let mut a = EntryBuilder::new(&keys, 0);
    let ea = a.next(Class::Edit, Frame(2), set(1));
    src.put(ea.clone());
    r.advance(Frame(15), &mut src);
    let cut = cut_over(10, &[&ea]);
    let mine = r.root_at_cut(&cut, &mut src).unwrap();
    let theirs = pe_seams::Hash([9; 32]);
    assert_eq!(
        r.finalize(&FinalCut { cut: cut.clone(), root: theirs, voters: vec![] }, &mut src),
        Finalized::Diverged { mine }
    );
    // Rung 1 from Cv (genesis) reproduces my root: the fault is not transient here.
    assert_eq!(r.replay_to(&cut, &mut src), Ok(mine));
}

#[test]
fn a_frame_no_longer_kept_from_the_floor_up_is_damaged_never_a_wait() {
    // S3 v1.2 round 3 (@chunks-reviewer's Medium): the time cap trims from the floor (genesis) up after the live
    // history stepped those frames, so nothing stops, but no root at a later cut can be taken again.
    let (keys, mut r, mut src) = setup();
    let mut a = EntryBuilder::new(&keys, 0);
    let ea = a.next(Class::Edit, Frame(2), set(1));
    src.put(ea.clone());
    r.advance(Frame(15), &mut src);
    let cut = cut_over(10, &[&ea]);
    let root = r.root_at_cut(&cut, &mut src).unwrap();
    src.kept = 5;
    assert_eq!(r.root_at_cut(&cut, &mut src), Err(RootError::Damaged));
    let fc = FinalCut { cut: cut.clone(), root, voters: vec![] };
    assert_eq!(r.finalize(&fc, &mut src), Finalized::Damaged, "a verdict, not NotReady for ever");
    // Rung 1 from Cv (genesis, below the trim) is refused before anything is restored.
    let live = r.state().root_hash();
    assert_eq!(r.replay_to(&cut, &mut src), Err(RootError::NotKept));
    assert_eq!(r.state().root_hash(), live);
}

#[test]
fn advance_stops_at_a_frame_no_longer_kept_and_never_steps_it_empty() {
    let (keys, mut r, mut src) = setup();
    let mut a = EntryBuilder::new(&keys, 0);
    src.put(a.next(Class::Edit, Frame(2), set(1)));
    r.advance(Frame(15), &mut src);
    src.kept = 5;
    // A change below the trim (S3 breaking its contract) sends advance back to frames no longer kept.
    let mut b = EntryBuilder::new(&keys, 1);
    src.put(b.next(Class::Edit, Frame(3), set(9)));
    let adv = r.advance(Frame(15), &mut src);
    assert!(!adv.caught_up, "{adv:?}");
    assert_eq!(r.frame(), Frame(3), "it stops at the first frame no longer kept");
}

#[test]
fn a_malformed_payload_is_refused_and_nothing_panics() {
    let (keys, mut r, mut src) = setup();
    let mut a = EntryBuilder::new(&keys, 0);
    let bad = a.next(Class::Edit, Frame(1), bytes::Bytes::from_static(&[0xFF, 1, 2]));
    src.put(bad.clone());
    r.advance(Frame(3), &mut src);
    assert!(matches!(r.outcome(&bad.id()), Some((_, Outcome::Refused { reason: Reason::Malformed, .. }))));
}

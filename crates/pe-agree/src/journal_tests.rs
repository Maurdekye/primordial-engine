//! Tests of the overflow journal (journal.rs), driven through the real storage seam on SimFs: offers, duplicates
//! and forks, the contiguous ack, the hot-memory cap with loads, finality and compaction, a full disk, the author's
//! cap, and a restart.

use std::path::PathBuf;

use bytes::Bytes;
use pe_log::ChainKey;
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{Class, Frame, SignedEntry};

use crate::journal::*;
use crate::store::*;

fn dir() -> PathBuf {
    PathBuf::from("data/id.agree/journal")
}

/// A journal, its log and the runner that applies its jobs inline (as the writer thread would).
struct Rig {
    j: Journal,
    log: Log<SimFs>,
    runner: Runner,
}

impl Rig {
    fn new(cfg: JournalConfig) -> Rig {
        let (log, _) = Log::open(SimFs::new(), &dir(), Options::default()).unwrap();
        Rig { j: Journal::new(cfg, 0), log, runner: Runner::new(0) }
    }

    /// Run the journal's jobs and feed back what the writer would report.
    fn flush(&mut self) {
        let jobs = self.j.take_jobs();
        let ran = self.runner.run(&mut self.log, &jobs);
        for (n, records) in ran.loaded {
            self.j.on_report(&Report::Loaded { n, records });
        }
        if let Some(through) = ran.durable {
            self.j.on_report(&Report::Durable { through, done: ran.done });
        }
        match ran.error {
            None => {}
            Some(StoreError::Full) => self.j.on_report(&Report::Full { from: self.runner.next }),
            Some(e) => panic!("{e}"),
        }
    }
}

fn entries(keys: &TestKeys, author: usize, n: usize, size: usize) -> Vec<SignedEntry> {
    let mut b = EntryBuilder::new(keys, author);
    (0..n).map(|i| b.next(Class::Input, Frame(i as u64 + 1), Bytes::from(vec![i as u8; size]))).collect()
}

fn ck(e: &SignedEntry) -> ChainKey {
    ChainKey::of(e)
}

#[test]
fn offers_dedup_and_the_contiguous_ack() {
    let keys = TestKeys::new(7, 2);
    let es = entries(&keys, 1, 6, 10);
    let mut r = Rig::new(JournalConfig::default());
    assert_eq!(r.j.offer(&es[0]), Offer::Taken);
    assert_eq!(r.j.offer(&es[0]), Offer::Duplicate);
    assert_eq!(r.j.offer(&es[1]), Offer::Taken);
    // Not durable yet: no ack.
    assert!(r.j.acks().is_empty());
    r.flush();
    assert_eq!(r.j.acks().get(&ck(&es[0])), Some(&2));
    // A hole at seq 4 holds the ack at 3.
    r.j.offer(&es[2]);
    r.j.offer(&es[4]);
    r.j.offer(&es[5]);
    r.flush();
    assert_eq!(r.j.acks().get(&ck(&es[0])), Some(&3));
    r.j.offer(&es[3]);
    r.flush();
    assert_eq!(r.j.acks().get(&ck(&es[0])), Some(&6));
}

#[test]
fn a_fork_keeps_both_entries() {
    let keys = TestKeys::new(8, 2);
    let mut b = EntryBuilder::new(&keys, 0);
    let first = b.next(Class::Input, Frame(1), Bytes::from_static(b"a"));
    let mut b2 = EntryBuilder::new(&keys, 0);
    let other = b2.next(Class::Input, Frame(1), Bytes::from_static(b"b"));
    assert_eq!((ck(&first), first.entry.seq), (ck(&other), other.entry.seq), "one chain, one seq");
    assert_ne!(first.id(), other.id());
    let mut r = Rig::new(JournalConfig::default());
    assert_eq!(r.j.offer(&first), Offer::Taken);
    assert_eq!(r.j.offer(&other), Offer::Taken);
    r.flush();
    assert_eq!(r.j.stats().entries, 2);
    let got = r.j.next(keys.key(0), 10);
    assert_eq!(got.len(), 2);
}

#[test]
fn final_entries_leave_and_are_refused() {
    let keys = TestKeys::new(9, 2);
    let es = entries(&keys, 0, 5, 10);
    let mut r = Rig::new(JournalConfig::default());
    for e in &es {
        r.j.offer(e);
    }
    r.flush();
    r.j.finalize([(ck(&es[0]), 3)], 0);
    assert_eq!(r.j.stats().entries, 2);
    assert_eq!(r.j.offer(&es[1]), Offer::Final);
    assert_eq!(r.j.acks().get(&ck(&es[0])), Some(&5));
    let got = r.j.next(keys.key(0), 10);
    assert_eq!(got.iter().map(|e| e.entry.seq).collect::<Vec<_>>(), vec![4, 5]);
}

#[test]
fn the_hot_cap_spills_to_disk_and_loads_back_in_order() {
    let keys = TestKeys::new(10, 2);
    let es = entries(&keys, 1, 200, 100);
    let size = es[0].encode().len();
    let cfg = JournalConfig { hot_per_author: size * 20, hot_total: size * 30, load_batch: 50, ..JournalConfig::default() };
    let mut r = Rig::new(cfg);
    for e in &es {
        assert_eq!(r.j.offer(e), Offer::Taken);
    }
    r.flush();
    let st = r.j.stats();
    assert!(st.hot_bytes <= size * 20, "hot {} > cap {}", st.hot_bytes, size * 20);
    // Take everything in chain order: hot first, then loads as asked for.
    let mut got: Vec<u64> = Vec::new();
    for _ in 0..50 {
        let batch = r.j.next(keys.key(1), 30);
        got.extend(batch.iter().map(|e| e.entry.seq));
        r.flush();
        if got.len() == es.len() {
            break;
        }
    }
    assert_eq!(got, (1..=200).collect::<Vec<u64>>(), "every entry once, in chain order");
    // Taken entries stay journalled (and acked) until final.
    assert_eq!(r.j.acks().get(&ck(&es[0])), Some(&200));
    assert_eq!(r.j.stats().hot_bytes, 0);
}

#[test]
fn compaction_keeps_live_entries_and_their_lsns() {
    let keys = TestKeys::new(11, 2);
    let es = entries(&keys, 0, 100, 200);
    let size = es[0].encode().len() as u64;
    let cfg = JournalConfig {
        compact_min_bytes: size * 10,
        compact_every_ns: 0,
        hot_per_author: (size * 5) as usize,
        ..JournalConfig::default()
    };
    let mut r = Rig::new(cfg);
    for e in &es {
        r.j.offer(e);
    }
    r.flush();
    let before = r.log.bytes().0;
    // An append still in flight when the compaction is asked for: it must survive it.
    let late = entries(&keys, 1, 1, 50);
    r.j.offer(&late[0]);
    r.j.finalize([(ck(&es[0]), 90)], 1);
    assert!(r.j.stats().in_flight >= 2, "the append and the compaction are queued");
    r.flush();
    assert!(r.log.bytes().0 < before / 3, "compacted: {} -> {}", before, r.log.bytes().0);
    assert_eq!(r.j.stats().dead_bytes, 0);
    // Cold ones load from their new places.
    let mut got: Vec<u64> = Vec::new();
    for _ in 0..10 {
        got.extend(r.j.next(keys.key(0), 100).iter().map(|e| e.entry.seq));
        r.flush();
    }
    assert_eq!(got, (91..=100).collect::<Vec<u64>>());
    assert_eq!(r.j.next(keys.key(1), 10).len(), 1, "the late entry survived");
    // And a restart finds exactly the live ones.
    let (log, rec) = Log::open(r.log.into_fs(), &dir(), Options::default()).unwrap();
    drop(log);
    let (_, back, bad) = Journal::recover(JournalConfig::default(), 0, &rec.records);
    assert_eq!(bad, 0);
    assert_eq!(back.len(), 11);
}

#[test]
fn a_full_disk_pauses_and_resumes() {
    let keys = TestKeys::new(12, 2);
    let es = entries(&keys, 0, 4, 100);
    let mut r = Rig::new(JournalConfig::default());
    r.j.offer(&es[0]);
    r.flush();
    let used: u64 = {
        let mut fs = r.log.fs().clone();
        fs.names().iter().map(|p| fs.read(p).map_or(0, |b| b.len() as u64)).sum()
    };
    r.log.fs_mut().capacity = Some(used + 10);
    assert_eq!(r.j.offer(&es[1]), Offer::Taken);
    r.flush();
    assert!(r.j.paused());
    assert_eq!(r.j.offer(&es[2]), Offer::Paused, "defer it in memory, as today");
    assert_eq!(r.j.acks().get(&ck(&es[0])), Some(&1), "the ack doesn't pass what isn't durable");
    r.log.fs_mut().capacity = None;
    r.flush();
    assert!(!r.j.paused());
    assert_eq!(r.j.acks().get(&ck(&es[0])), Some(&2));
    assert_eq!(r.j.offer(&es[2]), Offer::Taken);
}

#[test]
fn an_author_over_its_cap_pauses_for_that_author_only() {
    let keys = TestKeys::new(13, 2);
    let a = entries(&keys, 0, 10, 100);
    let b = entries(&keys, 1, 1, 100);
    let size = a[0].encode().len() as u64;
    let mut r = Rig::new(JournalConfig { max_per_author: size * 3, ..JournalConfig::default() });
    for e in &a[..3] {
        assert_eq!(r.j.offer(e), Offer::Taken);
    }
    assert_eq!(r.j.offer(&a[3]), Offer::Paused);
    assert_eq!(r.j.offer(&b[0]), Offer::Taken);
    r.flush();
    r.j.finalize([(ck(&a[0]), 2)], 0);
    assert_eq!(r.j.offer(&a[3]), Offer::Taken, "room again once entries are final");
}

#[test]
fn restart_recovers_the_index_and_forgets_on_request() {
    let keys = TestKeys::new(14, 3);
    let es = entries(&keys, 2, 5, 30);
    let mut r = Rig::new(JournalConfig::default());
    for e in &es {
        r.j.offer(e);
    }
    r.flush();
    // One more, never synced: a crash loses it.
    r.j.offer(&entries(&keys, 1, 1, 30)[0]);
    let crashed = r.log.fs().crash(|_, _| (0, None));
    let (log, rec) = Log::open(crashed, &dir(), Options::default()).unwrap();
    let (mut j, back, bad) = Journal::recover(JournalConfig::default(), 0, &rec.records);
    drop(log);
    assert_eq!(bad, 0);
    assert_eq!(back, es, "the entries, in log order, ids intact");
    assert_eq!(j.acks().get(&ck(&es[0])), Some(&5), "recovered entries are durable");
    // The caller's recheck refuses one (say its author left): forgetting it opens a hole in the ack.
    j.forget(ck(&es[2]), 3, es[2].id());
    assert_eq!(j.acks().get(&ck(&es[0])), Some(&2));
    assert_eq!(j.stats().entries, 4);
}

#[test]
fn a_record_that_is_not_an_entry_is_counted() {
    let rec = vec![Record { lsn: Lsn(5), kind: KIND_ENTRY, payload: b"not an entry".to_vec() }];
    let (j, back, bad) = Journal::recover(JournalConfig::default(), 0, &rec);
    assert_eq!((back.len(), bad, j.stats().entries), (0, 1, 0));
}

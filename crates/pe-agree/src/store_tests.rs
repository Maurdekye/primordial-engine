//! Tests of the storage seam (store.rs): round trips, a crash after every file operation with every torn length
//! and scrambled tails, corruption that must be refused, full disks, failed fsyncs, leftover segments and the
//! driver.

use std::path::{Path, PathBuf};

use crate::store::*;

fn dir() -> PathBuf {
    PathBuf::from("data/id.agree/safety")
}

type State = Vec<(u16, Vec<u8>)>;

fn state(r: &Recovered) -> State {
    r.records.iter().map(|r| (r.kind, r.payload.clone())).collect()
}

fn open(fs: SimFs) -> (Log<SimFs>, Recovered) {
    Log::open(fs, &dir(), Options::default()).expect("open")
}

/// A record whose bytes say where it came from.
fn rec(i: usize) -> (u16, Vec<u8>) {
    let len = [0, 1, 7, 40, 300][i % 5];
    ((i % 15 + 1) as u16, (0..len).map(|j| (i * 31 + j) as u8).collect())
}

#[derive(Clone, Debug)]
enum Op {
    Append(usize),
    Sync,
    /// Keep the last n records.
    Checkpoint(usize),
    /// Close and open again (after a sync, so nothing is lost).
    Reopen,
}

fn workload() -> Vec<Op> {
    use Op::*;
    vec![
        Append(0),
        Sync,
        Append(1),
        Append(2),
        Append(3),
        Sync,
        Append(4),
        Sync,
        Sync,
        Checkpoint(2),
        Append(5),
        Append(6),
        Sync,
        Reopen,
        Append(7),
        Sync,
        Checkpoint(0),
        Append(8),
        Append(9),
        Sync,
        Checkpoint(3),
        Reopen,
        Append(10),
        Sync,
        Append(11),
    ]
}

/// What a run went through: each state the log may hold after a crash, and the first one that must survive.
struct Run {
    fs: SimFs,
    history: Vec<State>,
    acked: usize,
    /// Whether the run reached its end with no crash.
    finished: bool,
}

/// Run the workload until the file system crashes (at `crash_at` operations, if given).
fn run(ops: &[Op], ntfs: bool, crash_at: Option<usize>) -> Run {
    let mut fs = SimFs::new();
    fs.ntfs = ntfs;
    fs.crash_at = crash_at;
    let mut history: Vec<State> = vec![Vec::new()];
    let mut acked = 0;
    let (mut log, r) = match Log::open(fs.clone(), &dir(), Options::default()) {
        Ok(x) => x,
        Err(_) => return Run { fs, history, acked, finished: false },
    };
    assert!(r.records.is_empty());
    let mut lsns: Vec<Lsn> = Vec::new();
    for op in ops {
        let cur = history.last().cloned().unwrap_or_default();
        let ok = match op {
            Op::Append(i) => {
                let (k, b) = rec(*i);
                let lsn = log.append(k, &b).expect("append never does IO");
                assert!(lsns.last().is_none_or(|l| *l < lsn), "Lsns grow");
                lsns.push(lsn);
                let mut next = cur.clone();
                next.push((k, b));
                history.push(next);
                true
            }
            Op::Sync => match log.sync() {
                Ok(d) => {
                    assert!(lsns.last().is_none_or(|l| *l <= d), "sync covers every append");
                    acked = history.len() - 1;
                    true
                }
                Err(_) => false,
            },
            Op::Checkpoint(n) => {
                let live: State = cur[cur.len().saturating_sub(*n)..].to_vec();
                history.push(live.clone());
                let refs: Vec<(u16, &[u8])> = live.iter().map(|(k, b)| (*k, b.as_slice())).collect();
                match log.checkpoint(&refs) {
                    Ok(l) => {
                        assert_eq!(l.len(), live.len());
                        assert!(lsns.last().is_none_or(|x| l.first().is_none_or(|f| f > x)), "Lsns grow");
                        lsns.extend(l);
                        acked = history.len() - 1;
                        true
                    }
                    Err(_) => false,
                }
            }
            Op::Reopen => {
                let f = log.into_fs();
                match Log::open(f.clone(), &dir(), Options::default()) {
                    Ok((l, r)) => {
                        assert_eq!(state(&r), cur, "a clean reopen gives back what was synced");
                        log = l;
                        true
                    }
                    Err(_) => {
                        return Run { fs: f, history, acked, finished: false };
                    }
                }
            }
        };
        if !ok {
            return Run { fs: log.into_fs(), history, acked, finished: false };
        }
    }
    Run { fs: log.into_fs(), history, acked, finished: true }
}

/// The torn states to try after a crash: every kept length of each file with volatile bytes, and scrambled tails.
fn tears(fs: &SimFs) -> Vec<(PathBuf, usize, Option<usize>)> {
    let v = fs.volatile();
    let mut out = vec![(PathBuf::new(), 0, None)];
    for (p, len) in v {
        for keep in 0..=len {
            out.push((p.clone(), keep, None));
        }
        // An unsynced write can reach the disk out of order: keep it all with one byte wrong, at several places.
        for g in (0..len).step_by((len / 9).max(1)) {
            out.push((p.clone(), len, Some(g)));
        }
    }
    out
}

fn recover_and_check(run: &Run, tear: &(PathBuf, usize, Option<usize>), ctx: &str) {
    let (path, keep, garble) = tear;
    let crashed = run.fs.crash(|p, _| if p == path.as_path() { (*keep, *garble) } else { (0, None) });
    let (mut log, r) = match Log::open(crashed, &dir(), Options::default()) {
        Ok(x) => x,
        Err(e) => panic!("{ctx}: recovery failed: {e}"),
    };
    let got = state(&r);
    let allowed = &run.history[run.acked..];
    assert!(
        allowed.contains(&got),
        "{ctx}: recovered {} records, not a state between the last ack and the last append",
        got.len()
    );
    // The recovered log takes new records and keeps them.
    log.append(9, b"after").expect("append");
    log.sync().expect("sync after recovery");
    let (_, r2) = open(log.into_fs());
    let mut want = got.clone();
    want.push((9, b"after".to_vec()));
    assert_eq!(state(&r2), want, "{ctx}: the recovered log keeps new records");
}

fn crash_grid(ntfs: bool) -> usize {
    let ops = workload();
    let clean = run(&ops, ntfs, None);
    assert!(clean.finished);
    let total = clean.fs.ops;
    let mut cases = 0;
    for k in 0..=total {
        let r = run(&ops, ntfs, Some(k));
        for t in tears(&r.fs) {
            recover_and_check(&r, &t, &format!("ntfs={ntfs} crash at op {k}, keep {} garble {:?}", t.1, t.2));
            cases += 1;
        }
    }
    assert!(total > 40, "the workload exercises the log ({total} operations)");
    cases
}

#[test]
fn crash_after_every_operation_ntfs() {
    let n = crash_grid(true);
    assert!(n > 1000, "{n} cases");
}

#[test]
fn crash_after_every_operation_posix() {
    let n = crash_grid(false);
    assert!(n > 1000, "{n} cases");
}

#[test]
fn round_trip_and_lsns() {
    let (mut log, r) = open(SimFs::new());
    assert!(r.records.is_empty());
    let a = log.append(1, b"one").unwrap();
    let b = log.append(2, b"").unwrap();
    assert!(a < b);
    assert!(log.durable() < a);
    let d = log.sync().unwrap();
    assert!(d >= b);
    let (log, r) = open(log.into_fs());
    assert_eq!(state(&r), vec![(1, b"one".to_vec()), (2, Vec::new())]);
    assert_eq!(r.records[0].lsn, a);
    assert_eq!(r.records[1].lsn, b);
    assert_eq!(r.torn_bytes, 0);
    drop(log);
}

#[test]
fn unsynced_records_are_lost_and_synced_ones_kept() {
    let (mut log, _) = open(SimFs::new());
    log.append(1, b"kept").unwrap();
    log.sync().unwrap();
    log.append(2, b"lost").unwrap();
    // Never written: the crash loses it whatever the disk kept.
    let (_, r) = open(log.fs().crash(|_, n| (n, None)));
    assert_eq!(state(&r), vec![(1, b"kept".to_vec())]);
}

#[test]
fn refused_records() {
    let (mut log, _) = Log::open(SimFs::new(), &dir(), Options { max_record: 8 }).unwrap();
    assert!(matches!(log.append(RESERVED_KINDS, b""), Err(StoreError::Refused(_))));
    assert!(matches!(log.append(1, &[0; 9]), Err(StoreError::Refused(_))));
    assert!(matches!(log.checkpoint(&[(0xFFFF, b"")]), Err(StoreError::Refused(_))));
    log.append(1, &[0; 8]).unwrap();
    log.sync().unwrap();
}

fn seg_path(fs: &SimFs) -> PathBuf {
    let names = fs.names();
    assert_eq!(names.len(), 1, "one segment: {names:?}");
    names[0].clone()
}

/// Flip the byte at `at` of the only segment.
fn flip(fs: &mut SimFs, at: usize) {
    let p = seg_path(fs);
    let b = fs.bytes_mut(&p).expect("no volatile bytes");
    b[at] ^= 1;
}

#[test]
fn damage_to_synced_data_is_corrupt() {
    let (mut log, _) = open(SimFs::new());
    log.append(1, b"first batch").unwrap();
    log.sync().unwrap();
    let (at_second, _) = log.bytes();
    log.append(2, b"second batch").unwrap();
    log.sync().unwrap();
    log.append(3, b"third batch").unwrap();
    log.sync().unwrap();
    let mut fs = log.into_fs();
    // A byte of the second batch's record: the third batch's commit marker proves it was synced.
    flip(&mut fs, at_second as usize + 40);
    match Log::open(fs, &dir(), Options::default()) {
        Err(StoreError::Corrupt { offset, .. }) => assert!(offset >= at_second),
        other => panic!("expected Corrupt, got {:?}", other.map(|x| x.1)),
    }
}

#[test]
fn damage_to_the_last_batch_reads_as_torn_until_reopened() {
    // The documented gap: no marker follows the last synced batch, so damage to it can't be told from a torn
    // write. Opening the log vouches for it at once, so after that the same damage is Corrupt.
    let (mut log, _) = open(SimFs::new());
    log.append(1, b"vouched").unwrap();
    log.sync().unwrap();
    let (at_last, _) = log.bytes();
    log.append(2, b"last batch").unwrap();
    log.sync().unwrap();
    let synced = log.into_fs();

    let mut damaged = synced.clone();
    flip(&mut damaged, at_last as usize + 40);
    let (_, r) = open(damaged);
    assert_eq!(state(&r), vec![(1, b"vouched".to_vec())]);
    assert!(r.torn_bytes > 0);

    let (log, r) = open(synced);
    assert_eq!(r.records.len(), 2);
    let mut fs = log.into_fs();
    flip(&mut fs, at_last as usize + 40);
    assert!(matches!(Log::open(fs, &dir(), Options::default()), Err(StoreError::Corrupt { .. })));
}

#[test]
fn damage_to_a_finished_checkpoint_is_corrupt() {
    let (mut log, _) = open(SimFs::new());
    log.append(1, b"old").unwrap();
    log.sync().unwrap();
    log.checkpoint(&[(4, b"live record")]).unwrap();
    let mut fs = log.into_fs();
    // In the live record, which the checkpoint's own commit marker follows.
    flip(&mut fs, 56 + 22 + 3);
    assert!(matches!(Log::open(fs, &dir(), Options::default()), Err(StoreError::Corrupt { .. })));
}

/// A commit marker for segment 1 at Lsn 1000, checked under `salt`, as a peer would have to build it.
fn forged_marker(salt: [u8; 16]) -> Vec<u8> {
    let lsn = 1000u64;
    let mut f = Vec::new();
    f.extend_from_slice(&8u32.to_le_bytes());
    f.extend_from_slice(&0xFFFEu16.to_le_bytes());
    f.extend_from_slice(&crate::store::check(crate::store::Seg { n: 1, salt }, lsn, 0xFFFE, &lsn.to_le_bytes()));
    f.extend_from_slice(&lsn.to_le_bytes());
    f
}

/// Damage the batch that carries `payload`, the last one synced: with no marker after it, it reads as torn.
fn damage_before(payload: &[u8]) -> Result<Recovered, StoreError> {
    let (mut log, _) = open(SimFs::new());
    log.append(1, b"vouched").unwrap();
    log.sync().unwrap();
    let (at, _) = log.bytes();
    log.append(20, payload).unwrap();
    log.sync().unwrap();
    let mut fs = log.into_fs();
    flip(&mut fs, at as usize + 2);
    Log::open(fs, &dir(), Options::default()).map(|x| x.1)
}

#[test]
fn a_peer_cannot_forge_a_marker() {
    // A journalled entry's bytes come from a peer. One shaped like a commit marker passes only under the
    // segment's salt, which never leaves the machine. SimFs's first salt is 1, then 0x11s.
    let mut salt = [0x11u8; 16];
    salt[..8].copy_from_slice(&1u64.to_le_bytes());
    let mut wrong = salt;
    wrong[15] ^= 1;
    let r = damage_before(&forged_marker(wrong)).expect("a guessed salt makes no marker: the damage reads as torn");
    assert_eq!(state(&r), vec![(1, b"vouched".to_vec())]);
    // The control: with the real salt the forgery would count, so the salt is what stops it.
    assert!(matches!(damage_before(&forged_marker(salt)), Err(StoreError::Corrupt { .. })));
}

#[test]
fn a_full_disk_keeps_records_buffered() {
    let (mut log, _) = open(SimFs::new());
    log.append(1, b"before").unwrap();
    log.sync().unwrap();
    let mut fs = log.into_fs();
    let used: u64 = fs.names().iter().map(|p| fs.read(p).map_or(0, |b| b.len() as u64)).sum();
    fs.capacity = Some(used + 30);
    let (mut log, _) = open(fs);
    let a = log.append(2, &[7; 64]).unwrap();
    assert!(matches!(log.sync(), Err(StoreError::Full)));
    assert!(!log.poisoned(), "the partial write was cut back");
    assert!(log.durable() < a);
    // Room again: the same records go through.
    let mut fs = log.fs().clone();
    fs.capacity = None;
    let (_, r) = open(fs.clone());
    assert_eq!(state(&r), vec![(1, b"before".to_vec())], "nothing of the failed batch is on disk");
    let (mut log, _) = open(fs);
    log.append(2, &[7; 64]).unwrap();
    log.sync().unwrap();
    let (_, r) = open(log.into_fs());
    assert_eq!(r.records.len(), 2);
}

#[test]
fn retry_after_full_in_place() {
    // The same log object retries once room is made (the writer thread does this).
    let mut fs = SimFs::new();
    fs.capacity = Some(10_000);
    let (log, _) = open(fs);
    let used = {
        let fs = log.fs().clone();
        fs.names().iter().map(|p| fs.clone().read(p).map_or(0, |b| b.len() as u64)).sum::<u64>()
    };
    let mut fs = log.into_fs();
    fs.capacity = Some(used + 40);
    let (mut log, _) = open(fs);
    log.append(3, &[1; 100]).unwrap();
    assert!(matches!(log.sync(), Err(StoreError::Full)));
    assert!(matches!(log.sync(), Err(StoreError::Full)), "still full");
    assert!(!log.poisoned());
}

#[test]
fn a_failed_fsync_poisons_until_reopened() {
    let (mut log, _) = open(SimFs::new());
    log.append(1, b"acked").unwrap();
    log.sync().unwrap();
    log.fs_mut().fail_next_sync = true;
    log.append(2, b"unknown").unwrap();
    let e = log.sync();
    assert!(e.is_err());
    assert!(log.poisoned());
    assert!(matches!(log.append(3, b"x"), Err(StoreError::Poisoned)));
    assert!(matches!(log.sync(), Err(StoreError::Poisoned)));
    assert!(matches!(log.checkpoint(&[]), Err(StoreError::Poisoned)));
    let (_, r) = open(log.fs().crash(|_, n| (n, None)));
    assert_eq!(r.records[0], Record { lsn: r.records[0].lsn, kind: 1, payload: b"acked".to_vec() });
}

#[test]
fn leftover_segments_are_harmless() {
    let (mut log, _) = open(SimFs::new());
    log.append(1, b"a").unwrap();
    log.sync().unwrap();
    let mut fs = log.into_fs();
    fs.refuse_remove = true;
    let (mut log, _) = open(fs);
    log.checkpoint(&[(2, b"b")]).unwrap();
    log.checkpoint(&[(3, b"c")]).unwrap();
    assert_eq!(log.fs().names().len(), 3, "the old segments couldn't be deleted");
    let mut fs = log.into_fs();
    let (log, r) = open(fs.clone());
    assert_eq!(state(&r), vec![(3, b"c".to_vec())], "the highest finished segment counts");
    drop(log);
    fs.refuse_remove = false;
    let (mut log, r) = open(fs);
    assert_eq!(r.removed_segments, 2);
    assert_eq!(log.fs().names().len(), 1);
    // And a later checkpoint never reuses a leftover's name.
    log.checkpoint(&[(4, b"d")]).unwrap();
    let (_, r) = open(log.into_fs());
    assert_eq!(state(&r), vec![(4, b"d".to_vec())]);
}

#[test]
fn a_half_written_checkpoint_never_counts() {
    // Crash inside the checkpoint at every operation: the log holds the old records or exactly the live set.
    for k in 0..12 {
        let (mut log, _) = open(SimFs::new());
        log.append(1, b"old").unwrap();
        log.sync().unwrap();
        let mut fs = log.into_fs();
        let start = fs.ops;
        fs.crash_at = Some(start + k);
        let (mut log, _) = match Log::open(fs.clone(), &dir(), Options::default()) {
            Ok(x) => x,
            Err(_) => continue,
        };
        let done = log.checkpoint(&[(2, b"new")]).is_ok();
        for keep_all in [false, true] {
            let (_, r) = open(log.fs().crash(|_, n| (if keep_all { n } else { 0 }, None)));
            let got = state(&r);
            if done {
                assert_eq!(got, vec![(2, b"new".to_vec())], "k={k}");
            } else {
                assert!(got == vec![(1, b"old".to_vec())] || got == vec![(2, b"new".to_vec())], "k={k}: {got:?}");
            }
        }
    }
}

#[test]
fn persist_runs_jobs_in_order() {
    let (mut log, _) = open(SimFs::new());
    let mut next = 0;
    let jobs = vec![
        Job::Append { n: 0, kind: 1, bytes: b"a".to_vec() },
        Job::Append { n: 1, kind: 2, bytes: b"b".to_vec() },
        Job::Checkpoint { n: 2, live: vec![(3, b"c".to_vec())] },
        Job::Append { n: 3, kind: 4, bytes: b"d".to_vec() },
    ];
    assert_eq!(persist(&mut log, &mut next, &jobs).unwrap(), Some(3));
    assert_eq!(next, 4);
    assert!(matches!(
        persist(&mut log, &mut next, &[Job::Append { n: 9, kind: 1, bytes: vec![] }]),
        Err(StoreError::Refused(_))
    ));
    assert_eq!(next, 4);
    let (_, r) = open(log.into_fs());
    assert_eq!(state(&r), vec![(3, b"c".to_vec()), (4, b"d".to_vec())]);
}

#[test]
fn persist_resumes_after_a_full_disk() {
    let (log, _) = open(SimFs::new());
    let mut fs = log.into_fs();
    let used: u64 = fs.names().iter().map(|p| fs.clone().read(p).map_or(0, |b| b.len() as u64)).sum();
    fs.capacity = Some(used + 60);
    let (mut log, _) = open(fs);
    let mut next = 0;
    let jobs: Vec<Job> = (0..3).map(|n| Job::Append { n, kind: 1, bytes: vec![n as u8; 20] }).collect();
    assert!(matches!(persist(&mut log, &mut next, &jobs), Err(StoreError::Full)));
    // Appends only buffer, so every job was taken; the sync is what failed, and a later one retries them.
    assert_eq!(next, 3);
    assert!(!log.poisoned());
}

#[test]
fn the_writer_thread_commits_in_order() {
    let (log, _) = open(SimFs::new());
    let w = Writer::spawn(log, 0, std::time::Duration::from_millis(5));
    w.submit(vec![Job::Append { n: 0, kind: 1, bytes: b"a".to_vec() }]);
    w.submit(vec![
        Job::Append { n: 1, kind: 1, bytes: b"b".to_vec() },
        Job::Append { n: 2, kind: 1, bytes: b"c".to_vec() },
    ]);
    let mut high = None;
    while high != Some(2) {
        match w.wait(std::time::Duration::from_secs(10)) {
            Some(Report::Durable { through: n, .. }) => {
                assert!(high.is_none_or(|h| n > h));
                high = Some(n);
            }
            other => panic!("{other:?}"),
        }
    }
    w.submit(vec![Job::Append { n: 7, kind: 1, bytes: vec![] }]);
    assert!(matches!(w.wait(std::time::Duration::from_secs(10)), Some(Report::Failed(StoreError::Refused(_)))));
    drop(w);
}

#[test]
fn names_that_are_not_segments_are_ignored() {
    let (mut log, _) = open(SimFs::new());
    log.append(1, b"x").unwrap();
    log.sync().unwrap();
    let mut fs = log.into_fs();
    for n in ["notes.txt", "00000000000000AB.seg", "1.seg", "000000000000000g.seg"] {
        let p = Path::new("data/id.agree/safety").join(n);
        let mut f = fs.create_new(&p).unwrap();
        fs.append(&mut f, b"junk").unwrap();
        fs.sync(&mut f).unwrap();
    }
    let (_, r) = open(fs);
    assert_eq!(state(&r), vec![(1, b"x".to_vec())]);
}

#[test]
fn records_read_back_by_lsn() {
    let (mut log, _) = open(SimFs::new());
    let a = log.append(1, b"alpha").unwrap();
    let b = log.append(2, &[3; 1000]).unwrap();
    // Not written yet: left out.
    assert!(log.read(&[a]).unwrap().is_empty());
    log.sync().unwrap();
    let got = log.read(&[b, a, Lsn(999)]).unwrap();
    assert_eq!(got.iter().map(|r| (r.lsn, r.kind)).collect::<Vec<_>>(), vec![(b, 2), (a, 1)]);
    assert_eq!(got[0].payload, vec![3; 1000]);
    // After a reopen, from the recovered index; after a checkpoint, only the new Lsns.
    let (mut log, _) = open(log.into_fs());
    assert_eq!(log.read(&[a]).unwrap().len(), 1);
    let new = log.checkpoint(&[(5, b"x")]).unwrap();
    assert!(log.read(&[a, b]).unwrap().is_empty());
    assert_eq!(log.read(&new).unwrap()[0].payload, b"x".to_vec());
}

#[test]
fn compaction_by_lsn_and_by_job() {
    let (log, _) = open(SimFs::new());
    let mut log = log;
    let mut r = Runner::new(0);
    let ran = r.run(
        &mut log,
        &[
            Job::Append { n: 0, kind: 1, bytes: b"drop".to_vec() },
            Job::Append { n: 1, kind: 1, bytes: b"keep by lsn".to_vec() },
        ],
    );
    assert!(ran.error.is_none());
    let keep_lsn = ran.done[1].lsns[0];
    // In one batch: an append whose Lsn the caller can't know yet, then a compaction that keeps it by job.
    let ran = r.run(
        &mut log,
        &[
            Job::Append { n: 2, kind: 1, bytes: b"keep by job".to_vec() },
            Job::Compact { n: 3, keep: vec![Keep::Lsn(keep_lsn), Keep::Job(2)] },
            Job::Load { n: 4, lsns: vec![] },
        ],
    );
    assert!(ran.error.is_none(), "{:?}", ran.error);
    assert_eq!(ran.durable, Some(4));
    let compact = ran.done.iter().find(|d| d.n == 3).unwrap();
    assert_eq!(compact.lsns.len(), 2);
    let (_, rec) = open(log.into_fs());
    assert_eq!(state(&rec), vec![(1, b"keep by lsn".to_vec()), (1, b"keep by job".to_vec())]);
    assert_eq!(rec.records.iter().map(|r| r.lsn).collect::<Vec<_>>(), compact.lsns);
}

#[test]
fn a_compaction_naming_an_unknown_job_is_refused() {
    let (mut log, _) = open(SimFs::new());
    let mut r = Runner::new(0);
    let ran = r.run(&mut log, &[Job::Compact { n: 0, keep: vec![Keep::Job(7)] }]);
    assert!(matches!(ran.error, Some(StoreError::Refused(_))));
}

#[test]
fn the_runner_reports_jobs_whose_sync_failed_once_a_later_one_succeeds() {
    let (log, _) = open(SimFs::new());
    let mut fs = log.into_fs();
    let used: u64 = fs.names().iter().map(|p| fs.clone().read(p).map_or(0, |b| b.len() as u64)).sum();
    fs.capacity = Some(used + 20);
    let (mut log, _) = open(fs);
    let mut r = Runner::new(0);
    let ran = r.run(&mut log, &[Job::Append { n: 0, kind: 1, bytes: vec![1; 50] }]);
    assert!(matches!(ran.error, Some(StoreError::Full)));
    assert!(ran.done.is_empty() && ran.durable.is_none());
    assert_eq!(r.next, 1, "taken: it stays buffered");
    log.fs_mut().capacity = None;
    let ran = r.run(&mut log, &[Job::Append { n: 1, kind: 1, bytes: vec![2; 5] }]);
    assert!(ran.error.is_none());
    assert_eq!(ran.done.iter().map(|d| d.n).collect::<Vec<_>>(), vec![0, 1], "job 0 is reported with job 1");
    assert_eq!(ran.durable, Some(1));
}

#[test]
fn the_writer_answers_loads() {
    let (log, _) = open(SimFs::new());
    let w = Writer::spawn(log, 0, std::time::Duration::from_millis(5));
    w.submit(vec![Job::Append { n: 0, kind: 3, bytes: b"x".to_vec() }]);
    let lsn = match w.wait(std::time::Duration::from_secs(10)) {
        Some(Report::Durable { through: 0, done }) => done[0].lsns[0],
        other => panic!("{other:?}"),
    };
    w.submit(vec![Job::Load { n: 1, lsns: vec![lsn] }]);
    match w.wait(std::time::Duration::from_secs(10)) {
        Some(Report::Loaded { n: 1, records }) => assert_eq!(records[0].payload, b"x".to_vec()),
        other => panic!("{other:?}"),
    }
}

// Regressions from @chunks-reviewer's review of 00e4db5e (m4dbcdae7a648; probes in its rv-store/review.rs).

/// SimFs, but the `fail_at`-th sync from now (1-based) fails, leaving its bytes volatile.
struct FailSync {
    fs: SimFs,
    syncs: usize,
    fail_at: Option<usize>,
}

impl Fs for FailSync {
    type File = SimHandle;
    fn create_dir_all(&mut self, d: &Path) -> std::io::Result<()> {
        self.fs.create_dir_all(d)
    }
    fn list(&mut self, d: &Path) -> std::io::Result<Vec<String>> {
        self.fs.list(d)
    }
    fn read(&mut self, p: &Path) -> std::io::Result<Vec<u8>> {
        self.fs.read(p)
    }
    fn read_at(&mut self, p: &Path, o: u64, l: usize) -> std::io::Result<Vec<u8>> {
        self.fs.read_at(p, o, l)
    }
    fn create_new(&mut self, p: &Path) -> std::io::Result<SimHandle> {
        self.fs.create_new(p)
    }
    fn open_append(&mut self, p: &Path) -> std::io::Result<SimHandle> {
        self.fs.open_append(p)
    }
    fn append(&mut self, f: &mut SimHandle, b: &[u8]) -> std::io::Result<()> {
        self.fs.append(f, b)
    }
    fn sync(&mut self, f: &mut SimHandle) -> std::io::Result<()> {
        self.syncs += 1;
        if self.fail_at == Some(self.syncs) {
            return Err(std::io::Error::other("fsync failed"));
        }
        self.fs.sync(f)
    }
    fn truncate(&mut self, f: &mut SimHandle, l: u64) -> std::io::Result<()> {
        self.fs.truncate(f, l)
    }
    fn remove(&mut self, p: &Path) -> std::io::Result<()> {
        self.fs.remove(p)
    }
    fn sync_dir(&mut self, d: &Path) -> std::io::Result<()> {
        self.fs.sync_dir(d)
    }
    fn salt(&mut self) -> [u8; 16] {
        self.fs.salt()
    }
}

fn three_batches() -> SimFs {
    let (mut log, _) = open(SimFs::new());
    for (k, b) in [(1, &b"vote state"[..]), (2, b"more vote state"), (3, b"third")] {
        log.append(k, b).unwrap();
        log.sync().unwrap();
    }
    log.into_fs()
}

fn used(fs: &SimFs) -> u64 {
    let mut fs = fs.clone();
    fs.names().iter().map(|p| fs.read(p).map_or(0, |b| b.len() as u64)).sum()
}

#[test]
fn damage_to_a_header_is_corrupt_and_kept() {
    // Every header field but the salt: the magic, the segment number, the base, the header's own check.
    for at in [0, 10, 20, 45] {
        let mut fs = three_batches();
        flip(&mut fs, at);
        match Log::open(fs.clone(), &dir(), Options::default()) {
            Err(StoreError::Corrupt { .. }) => {}
            other => panic!("byte {at}: expected Corrupt, got {:?}", other.map(|x| state(&x.1))),
        }
        assert_eq!(fs.names().len(), 1, "the evidence stays");
    }
}

#[test]
fn damage_to_a_salt_with_nothing_older_is_still_refused() {
    // The salt can't be proved damaged, but nothing counts while a segment is longer than a torn first one.
    let mut fs = three_batches();
    flip(&mut fs, 30);
    assert!(matches!(Log::open(fs, &dir(), Options::default()), Err(StoreError::Corrupt { .. })));
}

#[test]
fn damage_to_a_checkpoint_commit_with_no_predecessor_is_corrupt() {
    let (mut log, _) = open(SimFs::new());
    log.append(1, b"old").unwrap();
    log.sync().unwrap();
    log.checkpoint(&[(4, b"live vote state")]).unwrap();
    let mut fs = log.into_fs();
    let p = seg_path(&fs);
    let b = fs.bytes_mut(&p).unwrap();
    let n = b.len();
    b[n - 1] ^= 1;
    assert!(matches!(Log::open(fs.clone(), &dir(), Options::default()), Err(StoreError::Corrupt { .. })));
    assert_eq!(fs.names().len(), 1, "the evidence stays");
}

#[test]
fn a_stale_segment_is_never_fallen_back_to() {
    // Two checkpoints whose old segments couldn't be deleted (Windows), the middle one removed later, then damage
    // to the newest one's commit. Falling back to the oldest would bring back old vote state: a double vote.
    let (mut log, _) = open(SimFs::new());
    log.append(1, b"oldest").unwrap();
    log.sync().unwrap();
    log.fs_mut().refuse_remove = true;
    log.checkpoint(&[(2, b"middle")]).unwrap();
    log.append(3, b"after middle").unwrap();
    log.sync().unwrap();
    log.checkpoint(&[(4, b"newest")]).unwrap();
    let mut fs = log.into_fs();
    fs.refuse_remove = false;
    let names = fs.names();
    assert_eq!(names.len(), 3);
    fs.remove(&names[1]).unwrap();
    fs.sync_dir(&dir()).unwrap();
    let newest = fs.names()[1].clone();
    let b = fs.bytes_mut(&newest).unwrap();
    let n = b.len();
    b[n - 1] ^= 1;
    assert!(matches!(Log::open(fs, &dir(), Options::default()), Err(StoreError::Corrupt { .. })));
}

#[test]
fn the_real_predecessor_of_an_unfinished_checkpoint_still_counts() {
    // The control for the stale-segment rule: a checkpoint whose commit never reached the disk falls back to the
    // segment it replaced, which ends exactly at its base.
    let (log, _) = open(SimFs::new());
    let fs = log.into_fs();
    let (mut log, _) = Log::open(FailSync { fs, syncs: 0, fail_at: None }, &dir(), Options::default()).unwrap();
    log.append(1, b"kept").unwrap();
    log.sync().unwrap();
    log.fs_mut().syncs = 0;
    log.fs_mut().fail_at = Some(2);
    assert!(log.checkpoint(&[(2, b"new")]).is_err());
    // The commit's bytes stayed volatile, and the crash drops them: only the old segment counts.
    let crashed = log.into_fs().fs.crash(|_, _| (0, None));
    let (_, r) = open(crashed);
    assert_eq!(state(&r), vec![(1, b"kept".to_vec())]);
}

#[test]
fn a_failed_commit_fsync_in_a_checkpoint_poisons() {
    let (log, _) = open(SimFs::new());
    let fs = log.into_fs();
    let (mut log, _) = Log::open(FailSync { fs, syncs: 0, fail_at: None }, &dir(), Options::default()).unwrap();
    log.append(1, b"before").unwrap();
    log.sync().unwrap();
    // The checkpoint's own sync has nothing pending; write_segment syncs the body (1), then the commit (2).
    log.fs_mut().syncs = 0;
    log.fs_mut().fail_at = Some(2);
    assert!(log.checkpoint(&[(2, b"live")]).is_err());
    assert!(log.poisoned());
    assert!(matches!(log.append(3, b"never acked"), Err(StoreError::Poisoned)));
    // A reopen decides: the commit reached the file, so the new segment counts.
    let (_, r) = open(log.into_fs().fs);
    assert_eq!(state(&r), vec![(2, b"live".to_vec())]);
}

#[test]
fn a_failed_body_fsync_in_a_checkpoint_goes_on_in_the_old_segment() {
    let (log, _) = open(SimFs::new());
    let fs = log.into_fs();
    let (mut log, _) = Log::open(FailSync { fs, syncs: 0, fail_at: None }, &dir(), Options::default()).unwrap();
    log.append(1, b"before").unwrap();
    log.sync().unwrap();
    log.fs_mut().syncs = 0;
    log.fs_mut().fail_at = Some(1);
    assert!(log.checkpoint(&[(2, b"live")]).is_err());
    assert!(!log.poisoned());
    assert_eq!(log.fs().fs.names().len(), 1, "the half-written segment was removed");
    log.append(3, b"acked").unwrap();
    log.sync().unwrap();
    let (_, r) = open(log.into_fs().fs);
    assert_eq!(state(&r), vec![(1, b"before".to_vec()), (3, b"acked".to_vec())]);
}

#[test]
fn checkpoint_retries_on_a_full_disk_leave_no_files() {
    let (mut log, _) = open(SimFs::new());
    log.append(1, &[1; 200]).unwrap();
    log.sync().unwrap();
    let u = used(log.fs());
    log.fs_mut().capacity = Some(u + 150);
    for _ in 0..5 {
        assert!(matches!(log.checkpoint(&[(2, &[2; 200])]), Err(StoreError::Full)));
        assert!(!log.poisoned());
        assert_eq!(log.fs().names().len(), 1, "no leftover file");
    }
    // The freed space is still free: an append that fits goes through.
    log.append(3, &[3; 40]).unwrap();
    log.sync().unwrap();
}

#[test]
fn a_compaction_that_does_not_fit_is_skipped() {
    let (mut log, _) = open(SimFs::new());
    let mut r = Runner::new(0);
    let ran = r.run(&mut log, &[Job::Append { n: 0, kind: 1, bytes: vec![1; 200] }]);
    let lsn = ran.done[0].lsns[0];
    let u = used(log.fs());
    log.fs_mut().capacity = Some(u + 100);
    let ran = r.run(
        &mut log,
        &[Job::Compact { n: 1, keep: vec![Keep::Lsn(lsn)] }, Job::Append { n: 2, kind: 1, bytes: vec![2; 10] }],
    );
    assert!(ran.error.is_none(), "{:?}", ran.error);
    assert_eq!(ran.done.iter().map(|d| d.n).collect::<Vec<_>>(), vec![1, 2], "the jobs behind it ran");
    assert_eq!(ran.done[0].lsns, vec![lsn], "skipped: the record stays where it was");
    assert_eq!(log.fs().names().len(), 1);
}

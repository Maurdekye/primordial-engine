//! The storage seam on the real file system (StdFs), in cargo's per-target temporary folder: a round trip through a
//! checkpoint and a reopen, a segment that can't be deleted while another handle holds it open (Windows), and an
//! ignored timing run of append+sync.

use std::path::PathBuf;

use pe_agree::store::{Log, Options, StdFs};

fn fresh(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("store_std").join(name);
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn state(r: &pe_agree::store::Recovered) -> Vec<(u16, Vec<u8>)> {
    r.records.iter().map(|r| (r.kind, r.payload.clone())).collect()
}

#[test]
fn round_trip_through_a_checkpoint() {
    let d = fresh("round_trip");
    let (mut log, r) = Log::open(StdFs, &d, Options::default()).unwrap();
    assert!(r.records.is_empty());
    log.append(1, b"one").unwrap();
    log.append(2, &[9; 5000]).unwrap();
    log.sync().unwrap();
    drop(log);
    let (mut log, r) = Log::open(StdFs, &d, Options::default()).unwrap();
    assert_eq!(state(&r), vec![(1, b"one".to_vec()), (2, vec![9; 5000])]);
    log.checkpoint(&[(3, b"live")]).unwrap();
    log.append(4, b"after").unwrap();
    log.sync().unwrap();
    drop(log);
    let (_, r) = Log::open(StdFs, &d, Options::default()).unwrap();
    assert_eq!(state(&r), vec![(3, b"live".to_vec()), (4, b"after".to_vec())]);
    let segs = std::fs::read_dir(&d).unwrap().count();
    assert_eq!(segs, 1, "the old segment is gone");
}

#[test]
fn a_segment_held_open_elsewhere_is_left_and_removed_later() {
    let d = fresh("held_open");
    let (mut log, _) = Log::open(StdFs, &d, Options::default()).unwrap();
    log.append(1, b"old").unwrap();
    log.sync().unwrap();
    let old: Vec<PathBuf> = std::fs::read_dir(&d).unwrap().map(|e| e.unwrap().path()).collect();
    assert_eq!(old.len(), 1);
    // Another program holds the old segment open without delete sharing (a backup or a scanner, say), so on
    // Windows the delete fails. (std's own handles allow delete sharing.)
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(windows)]
    std::os::windows::fs::OpenOptionsExt::share_mode(&mut o, 3 /* FILE_SHARE_READ | FILE_SHARE_WRITE: the log still appends to it */);
    let held = o.open(&old[0]).unwrap();
    log.checkpoint(&[(2, b"new")]).unwrap();
    log.append(3, b"more").unwrap();
    log.sync().unwrap();
    drop(log);
    #[cfg(windows)]
    assert_eq!(std::fs::read_dir(&d).unwrap().count(), 2, "the held segment couldn't be deleted");
    let (log, r) = Log::open(StdFs, &d, Options::default()).unwrap();
    assert_eq!(state(&r), vec![(2, b"new".to_vec()), (3, b"more".to_vec())]);
    drop(log);
    drop(held);
    let (_, r) = Log::open(StdFs, &d, Options::default()).unwrap();
    assert_eq!(state(&r), vec![(2, b"new".to_vec()), (3, b"more".to_vec())]);
    assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1, "the leftover is removed once nothing holds it");
}

/// `cargo test -p pe-agree --test store_std -- --ignored --nocapture timing`: append+sync of one 120 B record (a
/// hand entry's size), 500 times, on the disk that holds the target folder.
#[test]
#[ignore]
fn timing() {
    let d = fresh("timing");
    let (mut log, _) = Log::open(StdFs, &d, Options::default()).unwrap();
    let rec = [7u8; 120];
    let mut ms: Vec<f64> = Vec::new();
    for _ in 0..500 {
        let t = std::time::Instant::now();
        log.append(16, &rec).unwrap();
        log.sync().unwrap();
        ms.push(t.elapsed().as_secs_f64() * 1e3);
    }
    ms.sort_by(f64::total_cmp);
    let at = |q: f64| ms[((ms.len() - 1) as f64 * q) as usize];
    println!(
        "append+sync of 120 B, {} runs at {}: median {:.2} ms, p90 {:.2}, p99 {:.2}, max {:.2}",
        ms.len(),
        d.display(),
        at(0.5),
        at(0.9),
        at(0.99),
        at(1.0)
    );
}

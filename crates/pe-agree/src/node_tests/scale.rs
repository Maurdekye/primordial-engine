//! Agreement at 3 to 8 members (M2 S1/S3, @item:primordial-engine/m2-s1-s3-agreement-at-3-8-players-open-internet-h):
//! the friend test runs at n = 2, and most of `node_tests` at n = 4. These run the same machinery at every n from 3
//! to 8: the final log agrees; as many crashes as the quorum spares are carried and one more stops finality; a cut-off
//! member catches up; members leave one by one and two at once; a late author and a withholding leader at n = 7; and
//! the friend test's per-frame load at n = 8.

use super::*;
use pe_seams::agree::quorum;

/// Member `i`'s present frame.
fn present(s: &Sim) -> Frame {
    s.clock.frame_at(HostTime(s.now)).unwrap()
}

/// The final logs of the members in `who` agree frame by frame below the lowest of their final frames; returns how
/// many final entries that is.
fn same_final_log_among(s: &Sim, who: &[usize]) -> usize {
    let low = who.iter().map(|&i| s.final_frame(i)).min().unwrap();
    let ids = |i: usize, f: u64| s.nodes[i].entries_at(Frame(f)).map(|v| v.iter().map(|e| e.id()).collect::<Vec<_>>());
    let mut count = 0;
    for f in 0..low.0 {
        let a = ids(who[0], f);
        for &i in &who[1..] {
            assert_eq!(a, ids(i, f), "members {} and {i} differ at frame {f}", who[0]);
        }
        count += a.map_or(0, |v| v.len());
    }
    count
}

fn no_faults(s: &Sim, who: &[usize], what: &str) {
    for &i in who {
        let faults: Vec<_> = s.events[i].iter().filter(|e| matches!(e, AgreeEvent::Fault(_))).collect();
        assert!(faults.is_empty(), "{what}: faults at member {i}: {faults:?}");
    }
}

fn sorted_keys(s: &Sim, who: &[usize]) -> Vec<MemberKey> {
    let mut v: Vec<MemberKey> = who.iter().map(|&i| s.keys.key(i)).collect();
    v.sort();
    v
}

#[test]
fn three_to_eight_members_agree_on_the_final_log() {
    for n in 3..=8u32 {
        for seed in [u64::from(n) * 10 + 1, u64::from(n) * 10 + 2] {
            let mut s = Sim::new(n, seed);
            let all: Vec<usize> = (0..n as usize).collect();
            let pause = longest_pause(&mut s, &all, 1_000, 4_000);
            eprintln!("n {n} seed {seed}: finality's longest pause 1-4 s: {pause} ms");
            let count = s.assert_same_final_log();
            let low = all.iter().map(|&i| s.final_frame(i)).min().unwrap();
            let lag_ms = (present(&s).0 - low.0) * 1000 / 60;
            eprintln!("n {n} seed {seed}: {count} final entries, the lowest final frame {} ms behind", lag_ms);
            // Each member submits 20 entries a second: nearly all of the first ~3.5 s are final everywhere.
            assert!(count >= n as usize * 60, "n {n} seed {seed}: only {count} final entries");
            assert!(lag_ms < 700, "n {n} seed {seed}: final {lag_ms} ms behind");
            for i in all {
                assert!(s.voids(i).is_empty(), "n {n} seed {seed}: honest entries voided at {i}: {:?}", s.voids(i));
            }
            no_faults(&s, &(0..n as usize).collect::<Vec<_>>(), &format!("n {n} seed {seed}"));
        }
    }
}

/// Run `s` to `until_ms` in 10 ms steps: the longest stretch (ms) in which the lowest final frame among `who` did
/// not move, from `from_ms` on.
fn longest_pause(s: &mut Sim, who: &[usize], from_ms: i64, until_ms: i64) -> i64 {
    let low = |s: &Sim| who.iter().map(|&i| s.final_frame(i)).min().unwrap();
    let (mut last, mut since, mut worst) = (low(s), s.now / MS, 0);
    while s.now / MS < until_ms {
        let t = s.now / MS + 10;
        s.run(t);
        let f = low(s);
        if f != last || t < from_ms {
            last = f;
            since = t;
        }
        worst = worst.max(t - since);
    }
    worst
}

/// Crash `crash` members at 1.5 s (the last ones, or with `first` the first ones) and run to 6 s: the live members'
/// final frame at 3 s and at the end, and how many final entries their logs share.
fn crash_run(n: u32, crash: usize, first: bool, seed: u64) -> (Frame, Frame, usize) {
    let mut s = Sim::new(n, seed);
    let down: Vec<usize> = if first { (0..crash).collect() } else { (n as usize - crash..n as usize).collect() };
    for &k in &down {
        s.down_from[k] = 1_500;
    }
    let live: Vec<usize> = (0..n as usize).filter(|i| !down.contains(i)).collect();
    let pause = longest_pause(&mut s, &live, 1_500, 3_000);
    eprintln!("n {n}, {crash} down (first: {first}): finality's longest pause 1.5-3 s: {pause} ms");
    let at3 = live.iter().map(|&i| s.final_frame(i)).min().unwrap();
    let pause = longest_pause(&mut s, &live, 3_000, 6_000);
    eprintln!("n {n}, {crash} down (first: {first}): finality's longest pause 3-6 s: {pause} ms");
    if std::env::var("PE_SCALE_LONG").is_ok() {
        let pause = longest_pause(&mut s, &live, 6_000, 20_000);
        eprintln!("n {n}, {crash} down (first: {first}): finality's longest pause 6-20 s: {pause} ms");
    }
    let end = live.iter().map(|&i| s.final_frame(i)).min().unwrap();
    let count = same_final_log_among(&s, &live);
    no_faults(&s, &live, &format!("n {n}, {crash} down"));
    (at3, end, count)
}

#[test]
fn the_quorum_bounds_how_many_may_crash_at_3_to_8() {
    // q = quorum(n) of n must vote, so n - q members may crash (at n = 3 and 6 that is more than t = floor((n-1)/3):
    // crashed members never equivocate, so safety only needs t Byzantine) and one more stops finality.
    for n in 3..=8u32 {
        let spare = n as usize - quorum(n as usize);
        for (seed, first) in [(u64::from(n) * 100 + 1, false), (u64::from(n) * 100 + 2, true)] {
            let (at3, end, count) = crash_run(n, spare, first, seed);
            eprintln!(
                "n {n}: {spare} down at 1.5 s (first: {first}): final {at3:?} at 3 s, {end:?} at 6 s, {count} shared"
            );
            assert!(end.0 >= 5 * 60, "n {n}, {spare} down (first: {first}): final only {end:?} at 6 s");
            let (at3, end, _) = crash_run(n, spare + 1, first, seed);
            eprintln!("n {n}: {} down at 1.5 s (first: {first}): final {at3:?} at 3 s, {end:?} at 6 s", spare + 1);
            assert!(at3.0 < 150 && end == at3, "n {n}, {} down (first: {first}): {at3:?} -> {end:?}", spare + 1);
        }
    }
}

#[test]
fn a_member_cut_off_for_two_seconds_catches_up_at_5_to_8() {
    for n in 5..=8u32 {
        let mut s = Sim::new(n, u64::from(n) * 1_000 + 7);
        s.partition = Some((1, 1_000, 3_000));
        s.run(3_000);
        let behind = s.final_frame(1);
        let others = s.final_frame(0);
        s.run(7_000);
        eprintln!("n {n}: at 3 s member 1 final {behind:?}, member 0 {others:?}; at 7 s {:?}", s.final_frame(1));
        assert!(others.0 >= 2 * 60, "n {n}: the others stalled while member 1 was cut off: {others:?}");
        assert!(s.final_frame(1).0 > behind.0 + 60, "n {n}: member 1 stuck at {behind:?}");
        let count = s.assert_same_final_log();
        assert!(count >= 100, "n {n}: only {count} final entries");
    }
}

#[test]
fn at_7_with_two_down_one_more_cut_off_stalls_finality_until_it_returns() {
    // n = 7, q = 5: with two members down the five live are exactly a quorum, so cutting one of them off for 2 s
    // stops finality, and its return restarts it.
    let mut s = Sim::new(7, 7_007);
    s.down_from[5] = 0;
    s.down_from[6] = 0;
    s.partition = Some((1, 2_000, 4_000));
    s.run(2_300);
    let before = s.final_frame(0);
    s.run(3_900);
    let during = s.final_frame(0);
    s.run(7_000);
    let live = [0, 1, 2, 3, 4];
    let after = live.iter().map(|&i| s.final_frame(i)).min().unwrap();
    eprintln!(
        "n 7, two down, member 1 cut off 2-4 s: final {before:?} at 2.3 s, {during:?} at 3.9 s, {after:?} at 7 s"
    );
    assert!(during.0 <= before.0 + 6, "finality went on without a quorum: {before:?} -> {during:?}");
    assert!(after.0 >= 6 * 60, "finality did not resume: {after:?}");
    same_final_log_among(&s, &live);
    no_faults(&s, &live, "n 7");
}

#[test]
fn members_leave_one_by_one_from_6_to_2() {
    let mut s = Sim::new(6, 66);
    s.run(2_000);
    let mut left = Vec::new();
    for k in (2..6).rev() {
        leave(&mut s, k);
        let t = s.now / MS;
        s.run(t + 2_000);
        let kept: Vec<usize> = (0..k).collect();
        for &i in &kept {
            assert_eq!(s.nodes[i].members(), sorted_keys(&s, &kept), "after {k} left: member {i}'s members");
        }
        assert!(s.nodes[k].leave_settled(), "member {k}'s leave not settled after 2 s");
        // The leaver quits.
        s.down_from[k] = s.now / MS;
        left.push(k);
        let t = s.now / MS;
        s.run(t + 1_000);
        let low = kept.iter().map(|&i| s.final_frame(i)).min().unwrap();
        let lag_ms = (present(&s).0 - low.0) * 1000 / 60;
        eprintln!("{} -> {}: the rest's final {lag_ms} ms behind", k + 1, k);
        assert!(lag_ms < 700, "after {k} left: final {lag_ms} ms behind");
        no_faults(&s, &kept, &format!("after {k} left"));
    }
    let all: Vec<usize> = (0..6).collect();
    let count = same_final_log_among(&s, &all);
    assert!(count > 100, "only {count} final entries everywhere");
    for i in 0..2 {
        assert!(s.voids(i).is_empty(), "honest entries voided at {i}: {:?}", s.voids(i));
    }
}

#[test]
fn two_members_leave_in_one_cut_at_6() {
    for seed in [61u64, 62] {
        let mut s = Sim::new(6, seed);
        s.run(2_000);
        leave(&mut s, 4);
        leave(&mut s, 5);
        s.run(4_000);
        let kept = [0, 1, 2, 3];
        for i in 0..6 {
            assert_eq!(s.nodes[i].members(), sorted_keys(&s, &kept), "seed {seed}: member {i}'s members");
        }
        assert!(s.nodes[4].leave_settled() && s.nodes[5].leave_settled(), "seed {seed}: leaves not settled");
        s.down_from[4] = 4_000;
        s.down_from[5] = 4_000;
        s.run(7_000);
        let low = kept.iter().map(|&i| s.final_frame(i)).min().unwrap();
        assert!(low.0 >= 6 * 60, "seed {seed}: the four didn't go on: {low:?}");
        same_final_log_among(&s, &(0..6).collect::<Vec<_>>());
        no_faults(&s, &kept, &format!("seed {seed}"));
    }
}

#[test]
fn a_late_author_is_voided_at_the_same_cut_everywhere_at_7() {
    let mut s = Sim::new(7, 75);
    s.stamp_lag[6] = 60;
    s.run(4_000);
    s.assert_same_final_log();
    let v0 = s.voids(0);
    assert!(!v0.is_empty(), "the lagging author's entries should be void as late");
    assert!(v0.iter().all(|v| v.1 == VoidReason::Late));
    for i in 1..7 {
        let vi = s.voids(i);
        let common = v0.len().min(vi.len());
        assert!(common > 0, "member {i} decided no void");
        assert_eq!(v0[..common], vi[..common], "member {i} decided a void differently");
    }
}

#[test]
fn a_leader_withholding_every_second_view_at_7_is_demoted_after_its_first() {
    for seed in [701u64, 702] {
        let mut s = Sim::new(7, seed);
        s.withhold = Some(6);
        s.run(20_000);
        let withheld = s.led.values().filter(|w| **w).count();
        let timeouts: Vec<i64> = (0..6).map(|i| s.nodes[i].core().timeout() / 1_000_000).collect();
        let final_s = s.final_frame(0).0 as f64 / 60.0;
        eprintln!(
            "seed {seed}: member 6 led {} views, withheld {withheld}; final at {final_s:.1} s of 20; timeouts {timeouts:?} ms",
            s.led.len()
        );
        assert!(s.led.len() <= 2 && withheld <= 1, "seed {seed}: member 6 led {:?}", s.led);
        assert!(final_s >= 19.0, "seed {seed}: final at {final_s:.1} s");
        assert!(timeouts.iter().all(|ms| *ms < 1_000), "seed {seed}: honest timeouts {timeouts:?} ms");
        s.assert_same_final_log();
    }
}

#[test]
fn eight_members_at_two_hands_per_frame_at_60_and_120_hz() {
    // The friend test's load (two hand entries a frame from every member) at n = 8: nothing is paced, voided or
    // refused, and finality keeps up.
    for hz in [60u32, 120] {
        let mut s = Sim::new(8, 800 + u64::from(hz));
        s.clock = Clock(hz);
        s.per_frame = vec![Some((2, 0)); 8];
        s.run(8_000);
        let count = s.assert_same_final_log();
        let low = (0..8).map(|i| s.final_frame(i)).min().unwrap();
        let lag_ms = (present(&s).0 - low.0) * 1000 / u64::from(hz);
        let paced = s.paced_frames.clone();
        eprintln!("n 8 at {hz} Hz: {count} final entries, final {lag_ms} ms behind, paced frames {paced:?}");
        assert!(lag_ms < 700, "{hz} Hz: final {lag_ms} ms behind");
        assert!(count >= 8 * 2 * hz as usize * 7, "{hz} Hz: only {count} final entries");
        for i in 0..8 {
            assert!(s.voids(i).is_empty(), "{hz} Hz: voided at {i}: {:?}", s.voids(i));
            assert_eq!(s.paced_frames[i], 0, "{hz} Hz: member {i} paced");
        }
        no_faults(&s, &(0..8).collect::<Vec<_>>(), &format!("{hz} Hz"));
    }
}

#[test]
fn with_two_down_at_6_to_8_the_reputation_window_keeps_finality_from_pausing() {
    // agreement.md 3 (coordinator): the window holds n - q = 2 leaders at n = 6-8, so once both dead
    // leaders are in it no view of theirs times out again. By 6 s each has timed out at most twice; from then on,
    // finality moves at least every 300 ms (healthy: 110-130 ms; the window of one paused 570-1,000 ms again and again).
    for n in 6..=8u32 {
        for (seed, first) in [(u64::from(n) * 100 + 1, false), (u64::from(n) * 100 + 2, true)] {
            let mut s = Sim::new(n, seed);
            let down: Vec<usize> = if first { vec![0, 1] } else { vec![n as usize - 2, n as usize - 1] };
            for &k in &down {
                s.down_from[k] = 1_500;
            }
            let live: Vec<usize> = (0..n as usize).filter(|i| !down.contains(i)).collect();
            s.run(6_000);
            let pause = longest_pause(&mut s, &live, 6_000, 12_000);
            eprintln!("n {n}, two down (first: {first}): finality's longest pause 6-12 s: {pause} ms");
            assert!(pause <= 300, "n {n}, two down (first: {first}): finality paused {pause} ms");
        }
    }
}

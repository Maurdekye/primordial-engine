//! R18's memory of the congested level (design 10.9, D56) and the explicit "no signal" (`set_link(None)`).

use pe_session::peer::SessionCore;
use pe_session::session::*;

fn core() -> SessionCore {
    let mut c = SessionCore::new(WorldSim::new(1, 1, &[1], SceneKind::BallPit { balls: 8, hands: 1 }, SimConfig::default(), KeyRing::test(1)));
    c.start(0);
    c
}

/// Run from `from_ms` to `to_ms` with the signal `q(ms)`; returns the level at every millisecond.
fn run(c: &mut SessionCore, from_ms: u64, to_ms: u64, q: impl Fn(u64) -> Option<u64>) -> Vec<usize> {
    let mut v = Vec::new();
    for ms in from_ms..to_ms {
        let now = ms * 1000;
        c.set_link(q(ms));
        c.begin_tick(now, false);
        c.end_tick(now);
        c.outbox();
        v.push(c.level);
    }
    v
}

/// The first millisecond (from `from_ms`) at which the level is `l`.
fn first_at(levels: &[usize], from_ms: u64, l: usize) -> Option<u64> {
    levels.iter().position(|&x| x == l).map(|i| from_ms + i as u64)
}

#[test]
fn it_lowers_back_into_the_congested_level_only_after_5_s_of_calm() {
    let mut c = core();
    assert_eq!((c.recall_calm_us, c.forget_us), (5_000_000, 30_000_000));
    // Calm at level 0, then congestion from 1 s: it begins at level 0, and the level climbs to 5
    // while the queue drains (those levels are not remembered).
    run(&mut c, 0, 1_000, |_| Some(0));
    let up = run(&mut c, 1_000, 1_700, |_| Some(60_000));
    assert_eq!(*up.last().unwrap(), 5);
    assert_eq!(c.congested_at, Some(0));
    // Calm: down to level 1 at the old pace (300 ms a level), then 5 s before level 0.
    let down = run(&mut c, 1_700, 9_000, |_| Some(0));
    let at1 = first_at(&down, 1_700, 1).unwrap();
    assert!(at1 < 1_700 + 4 * 300 + 100, "level 1 at {at1} ms");
    let at0 = first_at(&down, 1_700, 0).unwrap();
    assert!((at1 + 5_000..at1 + 5_100).contains(&at0), "level 0 at {at0} ms, level 1 at {at1} ms");
}

#[test]
fn congestion_beginning_at_a_higher_level_raises_the_memory() {
    let mut c = core();
    run(&mut c, 0, 500, |_| Some(60_000)); // begins at 0, climbs to 5
    run(&mut c, 500, 1_500, |_| Some(0)); // falls fast to 1
    assert_eq!(c.level, 1);
    assert_eq!(c.congested_at, Some(0));
    run(&mut c, 1_500, 1_520, |_| Some(60_000)); // begins again at level 1
    assert_eq!(c.congested_at, Some(1));
    let down = run(&mut c, 1_520, 9_000, |_| Some(0));
    // From 2 into 1 now also takes 5 s.
    let at2 = first_at(&down, 1_520, 2).unwrap();
    let at1 = first_at(&down, 1_520, 1).unwrap();
    assert!(at1 >= at2 + 5_000, "level 2 at {at2} ms, level 1 at {at1} ms");
}

#[test]
fn it_forgets_after_30_s_without_congestion() {
    let mut c = core();
    run(&mut c, 0, 300, |_| Some(60_000));
    assert_eq!(c.congested_at, Some(0));
    // 20 ms: neither congestion nor calm, so the level holds while the forget timer runs.
    let held = run(&mut c, 300, 30_200, |_| Some(20_000));
    assert!(held.iter().all(|&l| l == held[0]) && held[0] > 0, "the level held at {}", held[0]);
    assert_eq!(c.congested_at, Some(0), "29.9 s is not yet 30 s");
    run(&mut c, 30_200, 30_400, |_| Some(20_000));
    assert_eq!(c.congested_at, None, "forgotten after 30 s");
    // Calm now lowers at 300 ms a level all the way down.
    let from = c.level;
    let down = run(&mut c, 30_400, 30_400 + from as u64 * 300 + 200, |_| Some(0));
    assert_eq!(*down.last().unwrap(), 0);
}

#[test]
fn no_signal_holds_the_level_and_stops_the_forget_timer() {
    let mut c = core();
    run(&mut c, 0, 300, |_| Some(60_000));
    let l = c.level;
    assert!(l > 0);
    // 40 s without a signal: neither calm (no lowering) nor quiet (still remembered).
    let held = run(&mut c, 300, 40_300, |_| None);
    assert!(held.iter().all(|&x| x == l), "held at level {l}");
    assert_eq!(c.congested_at, Some(0));
    // A fresh calm signal lowers again.
    let down = run(&mut c, 40_300, 41_000, |_| Some(0));
    assert!(*down.last().unwrap() < l);
}

#[test]
fn a_core_without_a_signal_stays_at_level_0() {
    let mut c = core();
    let v = run(&mut c, 0, 2_000, |_| None);
    assert!(v.iter().all(|&l| l == 0));
}

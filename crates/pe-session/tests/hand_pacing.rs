//! R18 hand pacing (design 10.9, 6.10; D52, D53): while its author keeps sending, a hand never stops,
//! through every congestion level and every level change. A thinned entry is followed over its span,
//! then D52 coasts; at a level change the next pose takes the first frame aligned to the new stride
//! (shader-reviewer M1 on the ball-pit matrix: waiting for last + k left a 14-frame gap at 4 -> 5).
//! D73(a): under thinning the author's own hand is exact, as if every frame's target were sent.

use pe_session::peer::{AuthorError, SessionCore, HAND_STRIDE};
use pe_session::session::*;

#[test]
fn a_hand_never_stops_while_its_author_keeps_sending_through_every_level_change() {
    let balls = 8;
    let hand = balls as u32;
    let mut core = SessionCore::new(WorldSim::new(1, 1, &[1], SceneKind::BallPit { balls, hands: 1 }, SimConfig::default(), KeyRing::test(1)));
    assert!(core.adapt && core.ack_on_frames, "R18 is on by default");
    core.start(0);
    // This test is about pacing through level changes: D56's memory (tests/r18_memory.rs) would
    // stretch the schedule below, so it lowers after 300 ms of calm at every level here.
    core.recall_calm_us = 300_000;
    let mut last: Option<[f64; 3]> = None;
    let (mut frozen, mut moved, mut levels_seen, mut transitions) = (Vec::new(), 0, [false; 6], 0);
    let mut level = 0;
    let mut last_f = None;
    // The uplink queue: 50 ms (the level rises once per 100 ms to 5), then empty (it falls one level
    // per 300 ms back to 0), then up and down again.
    let delay = |ms: u64| if ms < 900 || (3_400..4_000).contains(&ms) { 50_000 } else { 0 };
    for ms in 0..6_500u64 {
        let now = ms * 1000;
        core.set_link(Some(delay(ms)));
        // D73(a): a pack's insertion rolls its author back, and the next tick reports its frame again.
        if let Some(f) = core.begin_tick(now, false).filter(|&f| last_f != Some(f)) {
            last_f = Some(f);
            // A target that keeps moving: the controller sweeps along x.
            let x = -600 + (f as i32 * 3) % 1200;
            match core.author(Payload::Hand { obj: hand, target_mm: [x, 400, 0], active: true, span: 1, more: Vec::new() }, 0) {
                Ok(_) | Err(AuthorError::Stride | AuthorError::Pending) => {}
                Err(e) => panic!("frame {f}: {e:?}"),
            }
            if core.level != level {
                transitions += 1;
                level = core.level;
            }
            levels_seen[core.level] = true;
            // The hand's position after the frame before: once it has moved, it must move every frame.
            let p = core.sim.obj_state(hand).unwrap().pos();
            if let Some(q) = last {
                if p == q && moved > 0 {
                    frozen.push((f, core.level));
                } else if p != q {
                    moved += 1;
                }
            }
            last = Some(p);
        }
        core.end_tick(now);
        core.outbox();
    }
    assert!(levels_seen.iter().all(|&b| b), "every level of {HAND_STRIDE:?} is used: {levels_seen:?}");
    assert!(transitions >= 20, "the level went up and down twice ({transitions} changes)");
    assert!(moved > 300, "the hand moved ({moved} frames)");
    assert!(frozen.is_empty(), "frozen frames (frame, level): {frozen:?}");
}

/// D73(a): the author's own hand, thinned through every R18 level and level change, is bit for bit
/// the hand of an unthinned author given the same per-frame targets, at every frame; and the packs
/// it sends give every frame of the stream exactly one target, so receivers can rebuild it.
#[test]
fn the_authors_hand_is_exact_at_every_level_and_its_packs_cover_every_frame() {
    let balls = 8;
    let hand = balls as u32;
    let mk = |adapt: bool| {
        let mut c = SessionCore::new(WorldSim::new(1, 1, &[1], SceneKind::BallPit { balls, hands: 1 }, SimConfig::default(), KeyRing::test(1)));
        c.adapt = adapt;
        // As above: D56's memory would stretch the level walk below.
        c.recall_calm_us = 300_000;
        c.start(0);
        c
    };
    let (mut thin, mut full) = (mk(true), mk(false));
    let delay = |ms: u64| if ms < 900 || (3_400..4_000).contains(&ms) { 50_000 } else { 0 };
    let (mut levels_seen, mut compared, mut packs) = ([false; 6], 0, 0);
    let mut sent: std::collections::BTreeMap<u64, [i32; 3]> = std::collections::BTreeMap::new();
    let mut wanted: std::collections::BTreeMap<u64, [i32; 3]> = std::collections::BTreeMap::new();
    let (mut at_thin, mut at_full) = (std::collections::BTreeMap::new(), std::collections::BTreeMap::new());
    for ms in 0..6_500u64 {
        let now = ms * 1000;
        thin.set_link(Some(delay(ms)));
        // The controller's target for a frame depends on the frame only, so both authors get the same.
        let target = |f: u64| [-600 + (f as i32 * 3) % 1200, 400 + (f as i32 % 50), 0];
        let pose = |f: u64| Payload::Hand { obj: hand, target_mm: target(f), active: true, span: 1, more: Vec::new() };
        if thin.begin_tick(now, false).is_some() {
            let s = thin.sim.frame.max(thin.sim.now);
            match thin.author(pose(s), 0) {
                Ok(_) | Err(AuthorError::Pending) => {
                    wanted.insert(s, target(s));
                }
                Err(AuthorError::Stride) => {}
                Err(e) => panic!("frame {s}: {e:?}"),
            }
            levels_seen[thin.level] = true;
        }
        if full.begin_tick(now, false).is_some() {
            let s = full.sim.frame.max(full.sim.now);
            match full.author(pose(s), 0) {
                Ok(_) | Err(AuthorError::Stride) => {}
                Err(e) => panic!("frame {s}: {e:?}"),
            }
        }
        thin.end_tick(now);
        full.end_tick(now);
        thin.outbox();
        full.outbox();
        // The hand after each simulated frame, the latest (after any rollback) kept.
        at_thin.insert(thin.sim.frame, (thin.sim.obj_state(hand), thin.level));
        at_full.insert(full.sim.frame, full.sim.obj_state(hand));
    }
    // What the author sent: its own hand entries, in its log.
    for e in thin.sim.all_entries().into_iter().filter(|e| e.member == 1) {
        if let Payload::Hand { target_mm, more, .. } = &e.payload {
            packs += !more.is_empty() as u32;
            for (j, t) in std::iter::once(target_mm).chain(more.iter()).enumerate() {
                assert!(sent.insert(e.stamp + j as u64, *t).is_none(), "frame {} sent twice", e.stamp + j as u64);
            }
        }
    }
    assert!(levels_seen.iter().all(|&b| b), "every level of {HAND_STRIDE:?} is used: {levels_seen:?}");
    assert!(packs > 50, "thinned poses went out in packs ({packs})");
    let mut mismatch = Vec::new();
    for (f, (st, level)) in &at_thin {
        if let Some(sf) = at_full.get(f) {
            compared += 1;
            if st != sf {
                mismatch.push((*f, *level));
            }
        }
    }
    assert!(compared > 350, "frames compared ({compared})");
    assert!(mismatch.is_empty(), "the author's hand differs from the unthinned author's at (frame, level): {:?}", &mismatch[..mismatch.len().min(10)]);
    // Every pose given, but the last pack's (still pending at the end), went out exactly once.
    let pending: u64 = wanted.keys().rev().take_while(|f| !sent.contains_key(f)).count() as u64;
    assert!(pending < PACK_MAX, "at most one pack pending at the end ({pending} poses)");
    for (f, t) in wanted.iter().take(wanted.len() - pending as usize) {
        assert_eq!(sent.get(f), Some(t), "frame {f}'s target");
    }
}

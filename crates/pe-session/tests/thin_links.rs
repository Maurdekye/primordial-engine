//! Thin links, Part A in pe-session (design 10.9; ported from the spike's p2/thin-link, main 11c20c1 with
//! reviews PA-2 and PA2-1): delta-coded pack targets, levels 6-8 with their climb rule, the drain level's
//! quick return, and single poses above the packed levels.

use pe_session::peer::{AuthorError, SessionCore, DEEP_QUEUE_US, DRAIN_LEVEL, HAND_STRIDE, PACKED_LEVELS};
use pe_session::session::*;

fn core() -> SessionCore {
    let mut c = SessionCore::new(WorldSim::new(1, 1, &[1], SceneKind::BallPit { balls: 8, hands: 1 }, SimConfig::default(), KeyRing::test(1)));
    c.start(0);
    c
}

/// Run from `from_ms` to `to_ms` with the signal `q(ms)`, the member active (a touch every frame);
/// returns the level at every millisecond.
fn run(c: &mut SessionCore, from_ms: u64, to_ms: u64, q: impl Fn(u64) -> Option<u64>) -> Vec<usize> {
    let mut v = Vec::new();
    for ms in from_ms..to_ms {
        let now = ms * 1000;
        c.set_link(q(ms));
        if c.begin_tick(now, false).is_some() {
            let _ = c.author(Payload::Touch { obj: (ms % 8) as u32, impulse: [0.0, 0.1, 0.0], grab: false }, 0);
        }
        c.end_tick(now);
        c.outbox();
        v.push(c.level);
    }
    v
}

fn hand(more: Vec<[i32; 3]>) -> Payload {
    Payload::Hand { obj: 8, target_mm: [0, 200, 500], active: true, span: 1, more }
}

/// The encoded bytes of a hand entry with these extra targets (a lone `Msg::Entry`).
fn wire(sim: &mut WorldSim, more: Vec<[i32; 3]>) -> Vec<u8> {
    let e = sim.author(hand(more), None);
    encode_msg(&Msg::Entry(e))
}

#[test]
fn pack_targets_go_as_deltas_in_the_narrowest_width_and_round_trip() {
    let mut sim = WorldSim::new(1, 1, &[1], SceneKind::BallPit { balls: 8, hands: 1 }, SimConfig::default(), KeyRing::test(1));
    let small: Vec<[i32; 3]> = (1..8).map(|i| [10 * i, 200, 500]).collect();
    let mid: Vec<[i32; 3]> = (1..8).map(|i| [1000 * i, 200, 500]).collect();
    let big: Vec<[i32; 3]> = (1..8).map(|i| [100_000 * i, 200, -500]).collect();
    assert_eq!(pack_delta_mode(&[0, 200, 500], &small), 1);
    assert_eq!(pack_delta_mode(&[0, 200, 500], &mid), 2);
    assert_eq!(pack_delta_mode(&[0, 200, 500], &big), 0);
    let (a, b, c) = (wire(&mut sim, small.clone()), wire(&mut sim, mid.clone()), wire(&mut sim, big.clone()));
    // 7 extra targets x 3 axes: 1, 2 or 4 bytes each, plus the width byte.
    assert_eq!((b.len() - a.len(), c.len() - a.len()), (21, 63));
    for (bytes, more) in [(a, small), (b, mid), (c, big)] {
        match decode_msg(&bytes) {
            Some(Msg::Entry(e)) => assert!(matches!(&e.payload, Payload::Hand { more: m, .. } if *m == more)),
            other => panic!("round trip: {other:?}"),
        }
    }
    // Extremes: a step of -128 needs i16, and i32 steps wrap without loss.
    let edge = vec![[-128, 200, 500], [i32::MAX, i32::MIN, 0]];
    assert_eq!(pack_delta_mode(&[0, 200, 500], &edge[..1]), 2);
    let d = wire(&mut sim, edge.clone());
    assert!(matches!(decode_msg(&d), Some(Msg::Entry(e)) if matches!(&e.payload, Payload::Hand { more: m, .. } if *m == edge)));
}

#[test]
fn a_pack_in_a_wider_width_than_the_narrowest_is_refused() {
    let mut sim = WorldSim::new(1, 1, &[1], SceneKind::BallPit { balls: 8, hands: 1 }, SimConfig::default(), KeyRing::test(1));
    let small: Vec<[i32; 3]> = (1..4).map(|i| [10 * i, 200, 500]).collect();
    let bytes = wire(&mut sim, small);
    // Layout of a lone hand pack: msg tag, member, seq, prev id, stamp, payload tag, obj, target, active,
    // span, count, width byte.
    let w = 1 + 4 + 4 + 32 + 8 + 1 + 4 + 12 + 1 + 1 + 1;
    assert_eq!(bytes[w], 1);
    let mut bad = bytes.clone();
    bad[w] = 7;
    assert!(decode_msg(&bad).is_none(), "an unknown width");
    // The same targets in i16: decodable values, but not the canonical width.
    let mut wide = bytes[..w].to_vec();
    wide.push(2);
    let mut prev = [0i32, 200, 500];
    for i in 1..4 {
        let t = [10 * i, 200, 500];
        for a in 0..3 {
            wide.extend_from_slice(&((t[a] - prev[a]) as i16).to_le_bytes());
        }
        prev = t;
    }
    wide.extend_from_slice(&bytes[w + 1 + 9..]);
    assert!(decode_msg(&wide).is_none(), "a non-canonical width");
}

#[test]
fn above_level_5_it_climbs_only_on_a_deep_or_growing_queue() {
    let mut c = core();
    // A steady 50 ms queue: 100 ms per level up to 5, and no further.
    let v = run(&mut c, 0, 3_000, |_| Some(50_000));
    assert_eq!(*v.iter().max().unwrap(), PACKED_LEVELS);
    // A deep queue climbs on, one level per 100 ms, to the coarsest.
    let v = run(&mut c, 3_000, 4_000, |_| Some(DEEP_QUEUE_US + 1));
    assert_eq!(*v.last().unwrap(), HAND_STRIDE.len() - 1);
    // A queue still growing climbs too, below the deep mark.
    let mut c = core();
    run(&mut c, 0, 1_000, |_| Some(50_000));
    let v = run(&mut c, 1_000, 1_400, |ms| Some(50_000 + (ms - 1_000) * 200));
    assert!(v.iter().any(|&l| l > PACKED_LEVELS), "a growing queue climbs past level 5");
}

#[test]
fn the_drain_level_returns_to_level_5_after_300_ms_of_calm_once_per_episode() {
    let mut c = core();
    run(&mut c, 0, 1_000, |_| Some(50_000));
    run(&mut c, 1_000, 1_050, |_| Some(DEEP_QUEUE_US + 1));
    assert_eq!(c.level, DRAIN_LEVEL);
    assert!(c.congested_at.is_some_and(|l| l <= PACKED_LEVELS), "until the quick return is used, D56 remembers at most level 5");
    // The first time: back to 5 after 300 ms of calm.
    let v = run(&mut c, 1_050, 1_600, |_| Some(0));
    assert_eq!(*v.last().unwrap(), PACKED_LEVELS);
    // Congested again in the same episode, from level 5: leaving the drain level now takes D56's 5 s of
    // calm while active, not 300 ms.
    run(&mut c, 1_600, 1_650, |_| Some(DEEP_QUEUE_US + 1));
    assert_eq!(c.level, DRAIN_LEVEL);
    assert_eq!(c.congested_at, Some(PACKED_LEVELS));
    let v = run(&mut c, 1_650, 6_000, |_| Some(0));
    assert!(v.iter().all(|&l| l == DRAIN_LEVEL), "no quick return twice in an episode");
    let v = run(&mut c, 6_000, 7_500, |_| Some(0));
    assert_eq!(*v.last().unwrap(), PACKED_LEVELS, "after the recall calm it steps down");
}

#[test]
fn above_level_5_a_hand_sends_single_poses_on_the_stride_ticks() {
    let mut c = core();
    c.recall_calm_us = 300_000;
    let h = 8u32;
    let (mut sent, mut skipped) = (Vec::new(), 0);
    for ms in 0..3_000u64 {
        let now = ms * 1000;
        c.set_link(Some(if ms < 1_000 { 50_000 } else { DEEP_QUEUE_US + 1 }));
        if let Some(f) = c.begin_tick(now, false) {
            if c.level == HAND_STRIDE.len() - 1 {
                match c.author(Payload::Hand { obj: h, target_mm: [f as i32, 400, 0], active: true, span: 1, more: Vec::new() }, 0) {
                    Ok(e) => sent.push((f, e.payload.clone())),
                    Err(AuthorError::Stride) => skipped += 1,
                    Err(e) => panic!("frame {f}: {e:?}"),
                }
            }
        }
        c.end_tick(now);
        c.outbox();
    }
    assert!(!sent.is_empty() && skipped > 0);
    let k = HAND_STRIDE[HAND_STRIDE.len() - 1];
    for (f, p) in &sent {
        assert_eq!(f % k, 0, "a pose on frame {f}, off the stride's ticks");
        assert!(matches!(p, Payload::Hand { span, more, .. } if *span as u64 == k && more.is_empty()), "{p:?}");
    }
}

/// The same as [`run`] with the member idle: it authors nothing, only its acks go out.
fn run_idle(c: &mut SessionCore, from_ms: u64, to_ms: u64, q: impl Fn(u64) -> Option<u64>) -> Vec<usize> {
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

/// A core at level 7 whose D56 memory covers levels 6 and 7: the quick return used, then congestion
/// beginning again at level 7.
fn at_level_7_remembered() -> SessionCore {
    let mut c = core();
    run(&mut c, 0, 1_000, |_| Some(50_000));
    run(&mut c, 1_000, 1_050, |_| Some(DEEP_QUEUE_US + 1));
    run(&mut c, 1_050, 1_600, |_| Some(0));
    assert_eq!(c.level, PACKED_LEVELS, "the quick return");
    run(&mut c, 1_600, 1_750, |_| Some(DEEP_QUEUE_US + 1));
    assert_eq!(c.level, 7);
    run(&mut c, 1_750, 1_760, |_| Some(0));
    run(&mut c, 1_760, 1_770, |_| Some(DEEP_QUEUE_US + 1));
    assert_eq!((c.level, c.congested_at), (7, Some(7)), "congestion beginning at level 7 is remembered");
    c
}

#[test]
fn above_level_5_an_idle_member_steps_down_at_300_ms_and_an_active_one_after_the_recall_calm() {
    // Reviews PA-2 / PA2-1: idle, it is back at packed level 5 within seconds, and rests there.
    let mut c = at_level_7_remembered();
    let v = run_idle(&mut c, 1_770, 6_000, |_| Some(0));
    assert_eq!(*v.last().unwrap(), PACKED_LEVELS);
    let v = run_idle(&mut c, 6_000, 12_000, |_| Some(0));
    assert!(v.iter().all(|&l| l == PACKED_LEVELS), "at level 5 and below idle calm does not count");
    // Active, the same calm needs D56's recall calm (5 s) for each remembered level.
    let mut c = at_level_7_remembered();
    let v = run(&mut c, 1_770, 6_000, |_| Some(0));
    assert!(v.iter().all(|&l| l == 7), "active: no step down before 5 s of calm");
    let v = run(&mut c, 6_000, 7_500, |_| Some(0));
    assert_eq!(*v.last().unwrap(), 6);
}

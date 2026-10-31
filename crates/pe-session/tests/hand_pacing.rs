//! R18 hand pacing (design 10.9, 6.10; D52, D53): while its author keeps sending, a hand never stops,
//! through every congestion level and every level change. A thinned entry is followed over its span,
//! then D52 coasts; at a level change the next pose takes the first frame aligned to the new stride
//! (shader-reviewer M1 on the ball-pit matrix: waiting for last + k left a 14-frame gap at 4 -> 5).

use pe_session::peer::{AuthorError, SessionCore, HAND_STRIDE};
use pe_session::session::*;

#[test]
fn a_hand_never_stops_while_its_author_keeps_sending_through_every_level_change() {
    let balls = 8;
    let hand = balls as u32;
    let mut core = SessionCore::new(WorldSim::new(1, 1, &[1], SceneKind::BallPit { balls, hands: 1 }, SimConfig::default(), KeyRing::test(1)));
    assert!(core.adapt && core.ack_on_frames, "R18 is on by default");
    core.start(0);
    let mut last: Option<[u32; 3]> = None;
    let (mut frozen, mut moved, mut levels_seen, mut transitions) = (Vec::new(), 0, [false; 6], 0);
    let mut level = 0;
    // The uplink queue: 50 ms (the level rises once per 100 ms to 5), then empty (it falls one level
    // per 300 ms back to 0), then up and down again.
    let delay = |ms: u64| if ms < 900 || (3_400..4_000).contains(&ms) { 50_000 } else { 0 };
    for ms in 0..6_500u64 {
        let now = ms * 1000;
        core.set_link(delay(ms));
        if let Some(f) = core.begin_tick(now, false) {
            // A target that keeps moving: the controller sweeps along x.
            let x = -600 + (f as i32 * 3) % 1200;
            match core.author(Payload::Hand { obj: hand, target_mm: [x, 400, 0], active: true, span: 1 }, 0) {
                Ok(_) | Err(AuthorError::Stride) => {}
                Err(e) => panic!("frame {f}: {e:?}"),
            }
            if core.level != level {
                transitions += 1;
                level = core.level;
            }
            levels_seen[core.level] = true;
            // The hand's position after the frame before: once it has moved, it must move every frame.
            let s = core.sim.obj_state(hand).unwrap();
            let p = [s.0[0], s.0[1], s.0[2]];
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

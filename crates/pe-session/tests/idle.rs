//! Design 7.6's idle predicate (reduction 2): a member is idle while every entry it holds, other than
//! acks, is stamped at or before its own confirmed frame.

use pe_session::peer::{AuthorError, SessionCore};
use pe_session::session::*;

#[test]
fn idle_until_an_entry_then_busy_until_it_is_confirmed() {
    // One member: its own acks confirm its frames as they pass.
    let mut c = SessionCore::new(WorldSim::new(1, 1, &[1], SceneKind::Pile { side: 2, layers: 1 }, SimConfig::default(), KeyRing::test(1)));
    c.start(0);
    let tick = |c: &mut SessionCore, ms: u64| {
        let now = ms * 1000;
        c.begin_tick(now, false);
        c.end_tick(now);
        c.outbox();
    };
    for ms in 0..500 {
        tick(&mut c, ms);
    }
    assert!(c.view().conf > 10, "frames confirm (conf {})", c.view().conf);
    assert!(c.idle(), "only acks so far: idle");
    // A touch is stamped after the confirmed frame: busy until the confirmed frame passes it.
    let e = c.author(Payload::Touch { obj: 1, impulse: [0.0, 1.0, 0.0], grab: false }, 0).expect("authored");
    assert!(e.stamp >= c.view().conf);
    assert!(!c.idle(), "busy while the touch is unconfirmed");
    let mut ms = 500;
    while c.view().conf <= e.stamp {
        tick(&mut c, ms);
        ms += 1;
        assert!(ms < 5_000, "the touch confirms");
    }
    assert!(c.idle(), "idle again once the touch is confirmed (conf {}, stamp {})", c.view().conf, e.stamp);
    // Busy again with the next entry.
    c.author(Payload::Touch { obj: 2, impulse: [1.0, 0.0, 0.0], grab: false }, 0).expect("authored");
    assert!(!c.idle(), "busy again with a new entry");
}

#[test]
fn an_unsent_hand_pack_keeps_a_member_busy() {
    let balls = 8;
    let hand = balls as u32;
    let mut c = SessionCore::new(WorldSim::new(1, 1, &[1], SceneKind::BallPit { balls, hands: 1 }, SimConfig::default(), KeyRing::test(1)));
    c.start(0);
    // R18 level 2: poses go out in packs of 3 frames (D73(a)). No link signal holds the level.
    c.level = 2;
    c.set_link(None);
    let mut ms = 0;
    let mut tick = |c: &mut SessionCore| {
        let now = ms * 1000;
        c.begin_tick(now, false);
        c.end_tick(now);
        c.outbox();
        ms += 1;
    };
    for _ in 0..500 {
        tick(&mut c);
    }
    assert!(c.idle(), "idle before any pose");
    // Author poses frame by frame until one is held in a pack: busy while it is held, though every
    // entry held is confirmed.
    let mut held = false;
    for _ in 0..200 {
        tick(&mut c);
        match c.author(Payload::Hand { obj: hand, target_mm: [0, 400, 0], active: true, span: 1, more: Vec::new() }, 0) {
            Err(AuthorError::Pending) => {
                held = true;
                break;
            }
            Ok(_) | Err(AuthorError::Stride) => {}
            Err(e) => panic!("{e:?}"),
        }
    }
    assert!(held, "a pose waits for its pack");
    assert!(c.view().hold_horizon_below.is_some());
    assert!(!c.idle(), "busy while a pack is unsent");
}

//! Design 7.6's idle predicate (reduction 2): a member is idle while every entry it holds, other than
//! acks, is stamped at or before its own confirmed frame.

use pe_session::peer::SessionCore;
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

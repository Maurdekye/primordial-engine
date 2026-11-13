//! render.md 14 test 6's run (src/ballpit.rs): pe-replica members over the network with a ball-pit push. A run keeps
//! its properties (P2 against a straight S2 replay of the final log among them), every member publishes views, the same (seed, delay) gives the same views, and once the pushes are
//! final every member shows the same poses.

use pe_harness::ballpit::{self, BallPit};
use pe_harness::clock::{MS, SEC};
use pe_harness::trace::Ev;
use pe_seams::{FrameView, Offset, Pos, ViewFlags, ViewObject};

fn small(seed: u64, delay_ms: u64) -> BallPit {
    BallPit::new(seed, delay_ms * MS, 3, 48, 4 * SEC, 400 * MS)
}

#[test]
fn a_ball_pit_run_keeps_its_properties_and_every_member_shows_the_same_poses() {
    let cfg = small(7, 50);
    assert!(cfg.pushes.len() >= 5, "{:?}", cfg.pushes);
    assert!(cfg.hand_pushes.len() >= 5, "{:?}", cfg.hand_pushes);
    let r = ballpit::run(&cfg).expect("the run builds");
    let vs = r.violations();
    assert!(vs.is_empty(), "{vs:#?}");
    // P2 compared real roots: the straight replay has one at every final frame (about one per 100 ms cut).
    let replay = r.replay();
    assert!(replay.len() >= 30, "the replay has {} roots", replay.len());
    for (n, views) in &r.views {
        assert!(views.len() > 100, "member {n} published {} views", views.len());
        assert!(views.windows(2).all(|w| w[0].0 <= w[1].0), "member {n}'s views in time order");
        assert!(views.iter().all(|(_, v)| v.objects.len() >= 48), "member {n}'s views draw every ball");
    }
    // Every push shows on every member. The pusher draws the lifted ball at its target (local-first). Another
    // member learns of the push a network delay and up to a cut later, so its first view of it may come after the
    // ball began to fall: it draws it in the air above the pile (0.4 to 0.9 m up, straight below the target).
    for p in &cfg.pushes {
        let ball = r.balls[p.ball];
        let to = Pos::ORIGIN.checked_add(Offset(p.to)).expect("in range");
        let mid = Pos::ORIGIN.checked_add(Offset([p.to[0], p.to[1] - 0.25, p.to[2]])).expect("in range");
        for (n, views) in &r.views {
            let (at, within) = if *n == p.member { (&to, 0.1) } else { (&mid, 0.25) };
            let seen = views.iter().any(|(_, v)| v.object(ball).is_some_and(|o| o.pos.dist2(at) < within * within));
            assert!(seen, "member {n} never shows ball {} lifted by member {} at {}", p.ball, p.member, p.at);
        }
    }
    // Each member marks exactly its own two hands as its own, and objects its rollbacks moved as corrected.
    let keys = ballpit::keys(cfg.members).sorted_keys();
    for (n, views) in &r.views {
        let me = keys[*n as usize - 1];
        for (_, v) in views {
            let own: Vec<_> = v.objects.iter().filter(|o| o.flags.contains(ViewFlags::OWN_HAND)).collect();
            assert_eq!(own.len(), 2, "member {n} marks {} hands as its own at {:?}", own.len(), v.frame);
            assert!(own.iter().all(|o| o.hand_of.is_some_and(|(m, _)| m == me)), "member {n}: another's hand marked");
        }
    }
    let corrected =
        |n| r.views[&n].iter().flat_map(|(_, v)| &v.objects).any(|o| o.flags.contains(ViewFlags::CORRECTED));
    assert!((1..=cfg.members).any(corrected), "no member's view marks a correction");
    // Every hand push shows on every member. The pusher's hand is at its target. The pusher (local-first) shows the
    // ball's upward speed gain over 1.5 m/s from the push's frame to the next (a change, so a ball already moving,
    // scattered by a landing lift, counts alike; design-reviewer's Low on 338073c1). The ball is in the air for about
    // 0.4 s, longer than a remote member takes to learn of the push, so every member shows it over 0.09 m above its
    // own height on the frame before the push (a resting ball's centre is 0.06 m up) within the 30 frames from it.
    let keys = ballpit::keys(cfg.members).sorted_keys();
    for p in &cfg.hand_pushes {
        let ball = r.balls[p.ball];
        let hand = ballpit::first_hand(&r.genesis, keys[p.member as usize - 1]).expect("the pusher has a hand");
        let target = ballpit::hand_target(&r.genesis, ball).expect("the ball has a place");
        let frame = p.at * u64::from(ballpit::HZ) / SEC;
        for (n, views) in &r.views {
            let at_target =
                views.iter().any(|(_, v)| v.object(hand).is_some_and(|o| o.pos.dist2(&target) < 0.05 * 0.05));
            assert!(
                at_target,
                "member {n} never shows member {}'s hand at its target for the push at {}",
                p.member, p.at
            );
            let newest = |f: u64| views.iter().rev().find(|(_, v)| v.frame.0 == f).and_then(|(_, v)| v.object(ball));
            if *n == p.member {
                let up = |f: u64| newest(f).map_or(f32::NAN, |o| o.linear_velocity[1]);
                let gain = up(frame + 1) - up(frame);
                assert!(gain > 1.5, "member {n}'s own push at {} gains ball {} {gain:.2} m/s upward", p.at, p.ball);
            }
            let height = |o: &ViewObject| (o.pos - Pos::ORIGIN).0[1];
            let before = newest(frame - 1).map(height).expect("a view of the frame before the push");
            let top = views
                .iter()
                .filter(|(_, v)| (frame..frame + 30).contains(&v.frame.0))
                .filter_map(|(_, v)| v.object(ball).map(height))
                .fold(f64::MIN, f64::max);
            assert!(
                top - before > 0.09,
                "member {n}: ball {} pushed by member {} at {} rises from {before:.3} m to {top:.3} m",
                p.ball,
                p.member,
                p.at
            );
        }
    }
    // Remote pushes arrive late, so members predicted without them and rolled back.
    let rollbacks = r.run.trace.records.iter().filter(|x| matches!(x.ev, Ev::RolledBack { .. })).count();
    assert!(rollbacks > 0, "no member rolled back");
    let (f, last) = r.last_common().expect("a frame every member published");
    assert!(f.0 >= 200, "the last common frame is near the end: {f:?}");
    // Flags are each member's own (its hands, its corrections); everything else must match exactly.
    let shown = |v: &FrameView| -> Vec<ViewObject> {
        v.objects.iter().map(|o| ViewObject { flags: ViewFlags::default(), ..o.clone() }).collect()
    };
    let (_, first) = &last[0];
    for (n, v) in &last[1..] {
        assert_eq!(shown(v), shown(first), "member {n} shows other objects than member {} at {f:?}", last[0].0);
    }
}

#[test]
fn the_same_seed_and_delay_give_the_same_views() {
    let a = ballpit::run(&small(3, 80)).expect("run a");
    let b = ballpit::run(&small(3, 80)).expect("run b");
    assert_eq!(a.run.trace.records, b.run.trace.records);
    for (n, va) in &a.views {
        let vb = &b.views[n];
        assert_eq!(va.len(), vb.len(), "member {n}");
        assert!(va.iter().zip(vb).all(|(x, y)| x.0 == y.0 && x.1.frame == y.1.frame && x.1.objects == y.1.objects));
    }
}

//! The in-frame apply order (coordinator) on PeWorld, over the network: a member's grab and its hand's
//! move are stamped in one frame, the grab first in agreed order (same stamp and author, lower seq). PeWorld applies a
//! frame's hand inputs before its other entries (`pe_replica::apply_order`, `PeWorld::phase`), so the grab's reach is
//! measured after the hand moved, and the grab holds, on every member and in P2's replay. Before the move the hand is
//! at its spawn point, 1.8 m from the pit: out of reach.

use pe_harness::ballpit::{self, BallPit, BallPitRun};
use pe_harness::clock::{MS, SEC};
use pe_seams::{Class, Offset};
use pe_world::Input;

/// The grabbing member, the ball, and when.
const MEMBER: u32 = 2;
const BALL: usize = 5;
const AT: u64 = SEC;
/// How far above the ball's centre the hand goes: 2 cm from its surface, within reach.
const ABOVE: f64 = 0.08;

/// A quiet pit (no pushes) where member 2 grabs ball 5 at 1 s and moves its hand next to it `hand_lag` frames later.
fn grab_run(hand_lag: u64) -> BallPitRun {
    let cfg = BallPit {
        seed: 5,
        delay_ns: 50 * MS,
        jitter_ns: 12 * MS,
        members: 3,
        balls: 24,
        end: 3 * SEC,
        pushes: Vec::new(),
        hand_pushes: Vec::new(),
    };
    let key = ballpit::keys(cfg.members).sorted_keys()[MEMBER as usize - 1];
    let script = move |id: u32, world: &pe_world::World, balls: &[pe_seams::ObjectId], node: ballpit::Member| {
        if id != MEMBER {
            return node;
        }
        let hand = ballpit::first_hand(world, key).expect("the member has a hand");
        let ball = balls[BALL];
        let at = world.store().world_transform(ball).expect("placed").pos;
        let to = at.checked_add(Offset([0.0, ABOVE, 0.0])).expect("in range");
        let lag = (hand_lag * SEC / u64::from(ballpit::HZ)) as i64;
        node.submit_at(AT as i64, Class::Input, ballpit::grab_payload(ball, hand)).submit_at(
            AT as i64 + lag,
            Class::Input,
            ballpit::hand_payload(hand, to),
        )
    };
    ballpit::run_with(&cfg, &script).expect("the run builds")
}

#[test]
fn a_grab_agreed_before_its_hands_move_in_one_frame_holds_on_every_member() {
    let r = grab_run(0);
    let vs = r.violations();
    assert!(vs.is_empty(), "{vs:#?}");
    let key = ballpit::keys(3).sorted_keys()[MEMBER as usize - 1];
    let hand = ballpit::first_hand(&r.genesis, key).expect("the member has a hand");
    let ball = r.balls[BALL];
    // Member 2's grab and hand input in the final log, in agreed order.
    let log = &r.finals[&1].entries;
    let find = |want: &dyn Fn(&Input) -> bool| {
        log.iter().position(|e| {
            e.entry.class == Class::Input
                && e.entry.author == key
                && Input::decode(&e.entry.payload).is_some_and(|i| want(&i))
        })
    };
    let g = find(&|i| matches!(i, Input::Grab { object, .. } if *object == ball)).expect("the grab is final");
    let h = find(&|i| matches!(i, Input::Hand { hand: id, .. } if *id == hand)).expect("the hand input is final");
    let stamp = log[g].entry.stamp.0;
    // Every member shows the ball held by member 2's hand within the claim's 500 ms.
    for (n, views) in &r.views {
        let held = views
            .iter()
            .filter(|(_, v)| (stamp + 1..=stamp + 25).contains(&v.frame.0))
            .any(|(_, v)| v.object(ball).and_then(|o| o.held_by).is_some_and(|h| h.member == key && h.hand == hand));
        assert!(held, "member {n} never shows ball {BALL} held by member {MEMBER}'s hand after frame {stamp}");
    }
    // ...and the grab was agreed before the hand input, in one frame: only the apply order made it reach.
    assert!(g < h, "agreed order: grab at {g}, hand at {h}");
    assert_eq!(log[h].entry.stamp.0, stamp, "one frame");
}

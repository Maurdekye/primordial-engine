//! The session and frame loop against the fake runtime (spec 10.1).

use std::sync::Arc;

use crate::backend::{Layers, SessionState, XrError, XrEvent};
use crate::fake::{Call, FakeRuntime, PERIOD_72, PERIOD_90, PERIOD_120};
use crate::hands::{GraspConfig, Reading};
use crate::ring::{DEFAULT_WINDOW_NS, HandRing};
use crate::seam::{Hand, HandFeed, HandSource, RigPose};
use crate::session::{FrameRequest, FrameSink, Step, XrSession};

#[derive(Default)]
struct Sink {
    frames: Vec<(i64, i64)>, // (display time, period)
    draw: bool,
}

impl FrameSink for Sink {
    fn render(&mut self, f: &FrameRequest<'_>) -> bool {
        self.frames.push((f.timing.predicted_display_time.0, f.timing.predicted_display_period_ns));
        self.draw
    }
}

fn sink() -> Sink {
    Sink { frames: Vec::new(), draw: true }
}

fn session(rt: FakeRuntime) -> (XrSession<FakeRuntime>, Arc<HandRing>) {
    let ring = Arc::new(HandRing::new(DEFAULT_WINDOW_NS, PERIOD_120));
    (XrSession::new(rt, ring.clone(), GraspConfig::default()), ring)
}

fn run(s: &mut XrSession<FakeRuntime>, sink: &mut Sink, n: usize) -> Vec<Step> {
    (0..n).map(|_| s.step(sink)).collect()
}

fn no_violations(s: &XrSession<FakeRuntime>) {
    assert!(s.backend().violations.is_empty(), "{:#?}", s.backend().violations);
}

#[test]
fn idle_before_ready_runs_no_frame() {
    let (mut s, _) = session(FakeRuntime::new());
    let mut k = sink();
    assert_eq!(run(&mut s, &mut k, 3), vec![Step::Idle; 3]);
    assert_eq!(s.backend().count(&Call::WaitFrame), 0);
    no_violations(&s);
}

#[test]
fn a_full_lifecycle_with_a_restart_on_the_same_session_and_an_exit() {
    let (mut s, _) = session(FakeRuntime::new().started());
    let mut k = sink();
    assert!(run(&mut s, &mut k, 10).iter().all(|x| *x == Step::Frame));
    assert_eq!(s.state(), SessionState::Focused);

    // The headset sleeps: STOPPING, then IDLE. The session is kept, not destroyed.
    for st in [SessionState::Visible, SessionState::Synchronized, SessionState::Stopping, SessionState::Idle] {
        s.backend_mut().push(XrEvent::StateChanged(st));
    }
    assert_eq!(s.step(&mut k), Step::Idle);
    assert_eq!(s.backend().count(&Call::EndSession), 1);
    assert_eq!(s.backend().count(&Call::DestroySession), 0, "STOPPING keeps the session");
    assert_eq!(s.step(&mut k), Step::Idle);

    // It wakes: READY again on the same session.
    for st in [SessionState::Ready, SessionState::Synchronized, SessionState::Visible, SessionState::Focused] {
        s.backend_mut().push(XrEvent::StateChanged(st));
    }
    assert_eq!(s.step(&mut k), Step::Frame);
    assert_eq!(s.backend().count(&Call::BeginSession), 2);

    // Leaving VR by choice: request exit, and the runtime walks down to EXITING.
    s.request_exit().unwrap();
    let mut steps = Vec::new();
    for _ in 0..5 {
        steps.push(s.step(&mut k));
    }
    assert_eq!(*steps.last().unwrap(), Step::Ended { lost: false });
    assert_eq!(s.backend().count(&Call::EndSession), 2);
    assert_eq!(s.backend().count(&Call::DestroySession), 1);
    no_violations(&s);
}

#[test]
fn loss_pending_tears_down_once_and_calls_nothing_after() {
    let (mut s, _) = session(FakeRuntime::new().started().at(5, XrEvent::StateChanged(SessionState::LossPending)));
    let mut k = sink();
    let steps = run(&mut s, &mut k, 8);
    assert_eq!(steps[..5], [Step::Frame; 5]);
    assert_eq!(steps[5..], [Step::Ended { lost: true }; 3]);
    let calls = &s.backend().calls;
    let destroy = calls.iter().position(|c| *c == Call::DestroySession).unwrap();
    assert_eq!(calls.len(), destroy + 1, "nothing after the teardown: {:?}", &calls[destroy..]);
    assert_eq!(s.backend().count(&Call::DestroySession), 1);
    no_violations(&s);
}

#[test]
fn a_lost_session_mid_frame_ends_cleanly() {
    let mut rt = FakeRuntime::new().started();
    rt.fail_wait_frame = Some((4, XrError::SessionLost));
    let (mut s, ring) = session(rt);
    let mut k = sink();
    let steps = run(&mut s, &mut k, 5);
    assert_eq!(steps[3], Step::Ended { lost: true });
    assert_eq!(s.backend().count(&Call::DestroySession), 1);
    assert_eq!(ring.hand_at(Hand::Right, s.backend().now_for_test()), None, "hands absent after the loss");
    no_violations(&s);
}

#[test]
fn every_frame_waits_begins_and_ends_in_order_with_one_layer() {
    let (mut s, _) = session(FakeRuntime::new().started());
    let mut k = sink();
    run(&mut s, &mut k, 6);
    let frame_calls: Vec<&Call> =
        s.backend().calls.iter().filter(|c| !matches!(c, Call::PollEvent | Call::BeginSession)).collect();
    // One frame: wait, begin, sync, both hands, acquire, locate, release, end.
    let one = |c: &[&Call]| {
        matches!(
            c,
            [
                Call::WaitFrame,
                Call::BeginFrame,
                Call::SyncActions,
                Call::ReadHand(Hand::Left),
                Call::ReadHand(Hand::Right),
                Call::AcquireImage,
                Call::LocateViews(_),
                Call::ReleaseImage,
                Call::EndFrame(Layers::Projection { with_depth: false }),
            ]
        )
    };
    assert_eq!(frame_calls.len(), 6 * 9);
    assert!(frame_calls.chunks(9).all(one), "{frame_calls:#?}");
    // Views are located at the display time the frame was given.
    for (c, (t, _)) in frame_calls.chunks(9).zip(&k.frames) {
        assert_eq!(*c[6], Call::LocateViews(crate::seam::HostTime(*t)));
    }
    no_violations(&s);
}

#[test]
fn should_render_false_submits_no_layer_but_still_samples_the_hands() {
    let mut rt = FakeRuntime::new().started();
    rt.not_rendered = vec![2, 3];
    let (mut s, _) = session(rt);
    let mut k = sink();
    run(&mut s, &mut k, 4);
    assert_eq!(s.backend().count(&Call::EndFrame(Layers::None)), 2);
    assert_eq!(s.backend().count(&Call::AcquireImage), 2);
    assert_eq!(s.backend().count(&Call::SyncActions), 4);
    assert_eq!(s.backend().count(&Call::ReadHand(Hand::Left)), 4);
    assert_eq!(s.counters.frames_not_rendered, 2);
    no_violations(&s);
}

#[test]
fn a_sink_that_draws_nothing_ends_the_frame_with_no_layer() {
    let (mut s, _) = session(FakeRuntime::new().started());
    let mut k = Sink { frames: Vec::new(), draw: false };
    run(&mut s, &mut k, 2);
    assert_eq!(s.backend().count(&Call::EndFrame(Layers::None)), 2);
    assert_eq!(s.backend().count(&Call::ReleaseImage), 2, "the image is still released");
    no_violations(&s);
}

#[test]
fn hands_reach_the_ring_while_focused_and_go_absent_without_focus() {
    let (mut s, ring) = session(FakeRuntime::new().started());
    let mut k = sink();
    run(&mut s, &mut k, 3);
    let now = s.backend().now_for_test();
    let h = ring.hand_at(Hand::Right, now).unwrap();
    assert!(h.tracked);
    assert_eq!(h.pose.position, [0.2, 1.0, -0.3]);

    s.backend_mut().push(XrEvent::StateChanged(SessionState::Visible));
    run(&mut s, &mut k, 2);
    assert_eq!(ring.hand_at(Hand::Right, s.backend().now_for_test()), None, "absent while not focused");
    let syncs = s.backend().count(&Call::SyncActions);
    run(&mut s, &mut k, 2);
    assert_eq!(s.backend().count(&Call::SyncActions), syncs, "no sync while VISIBLE");

    s.backend_mut().push(XrEvent::StateChanged(SessionState::Focused));
    run(&mut s, &mut k, 1);
    assert!(ring.hand_at(Hand::Right, s.backend().now_for_test()).is_some(), "back with focus");
    no_violations(&s);
}

#[test]
fn a_recentre_bumps_the_discontinuity_of_both_hands() {
    let (mut s, ring) = session(FakeRuntime::new().started());
    let mut k = sink();
    run(&mut s, &mut k, 2);
    let before = ring.hand_at(Hand::Left, s.backend().now_for_test()).unwrap().discontinuity;
    s.backend_mut().push(XrEvent::ReferenceSpaceChangePending);
    run(&mut s, &mut k, 1);
    let now = s.backend().now_for_test();
    assert_eq!(ring.hand_at(Hand::Left, now).unwrap().discontinuity, before + 1);
    assert_eq!(ring.hand_at(Hand::Right, now).unwrap().discontinuity, before + 1);
    assert_eq!(s.counters.recentres, 1);
    no_violations(&s);
}

#[test]
fn pacing_follows_whatever_period_the_runtime_gives_including_a_change_mid_run() {
    // 120 Hz, then 90 from frame 5, then 72 from frame 9: no target rate, no request (spec 7.2).
    let rt = FakeRuntime::new().started().period_from(5, PERIOD_90).period_from(9, PERIOD_72);
    let (mut s, ring) = session(rt);
    let mut k = sink();
    assert!(run(&mut s, &mut k, 12).iter().all(|x| *x == Step::Frame));
    let periods: Vec<i64> = k.frames.iter().map(|f| f.1).collect();
    assert_eq!(&periods[..4], &[PERIOD_120; 4]);
    assert_eq!(&periods[4..8], &[PERIOD_90; 4]);
    assert_eq!(&periods[8..], &[PERIOD_72; 4]);
    // Display times step by exactly the period in force.
    for w in k.frames.windows(2) {
        assert_eq!(w[1].0 - w[0].0, w[1].1);
    }
    assert_eq!(s.counters.display_frames, 12);
    assert_eq!(s.counters.missed_frames, 0);
    assert_eq!(s.counters.last_display_period_ns, PERIOD_72);
    // The ring's extrapolation limit followed the period: one 72 Hz period now.
    let now = s.backend().now_for_test();
    let mut moving = ring.hand_at(Hand::Right, now).unwrap();
    moving.linear_velocity = Some([1.0, 0.0, 0.0]);
    moving.sampled_at = now.plus_nanos(1);
    ring.push(moving);
    let far = ring.hand_at(Hand::Right, now.plus_nanos(1_000_000_000)).unwrap();
    let moved = far.pose.position[0] - 0.2;
    assert!((moved - PERIOD_72 as f32 * 1e-9).abs() < 1e-5, "{moved}");
    no_violations(&s);
}

#[test]
fn a_late_display_time_counts_as_a_missed_frame() {
    let (mut s, _) = session(FakeRuntime::new().started());
    let mut k = sink();
    run(&mut s, &mut k, 3);
    s.backend_mut().skip_next_ns = PERIOD_120; // the next display time comes two periods later
    run(&mut s, &mut k, 2);
    assert_eq!(s.counters.missed_frames, 1);
    no_violations(&s);
}

#[test]
fn a_held_controller_releases_after_the_grace_period_when_tracking_is_lost() {
    // The right controller squeezes, then loses tracking for 400 ms.
    let rt = FakeRuntime::new().started().hands(move |hand, at| {
        let t_ms = (at.0 - 1_000_000_000) / 1_000_000;
        let lost = hand == Hand::Right && (100..500).contains(&t_ms);
        Reading {
            source: HandSource::Controller,
            at,
            pose: (!lost).then_some(RigPose { position: [0.2, 1.0, -0.3], orientation: [0.0, 0.0, 0.0, 1.0] }),
            linear_velocity: None,
            angular_velocity: None,
            grip: Some(if hand == Hand::Right { 0.9 } else { 0.0 }),
        }
    });
    let (mut s, ring) = session(rt);
    let mut k = sink();
    let mut closed_at = Vec::new();
    for _ in 0..80 {
        s.step(&mut k);
        let now = s.backend().now_for_test();
        let h = ring.hand_at(Hand::Right, now).unwrap();
        closed_at.push(((now.0 - 1_000_000_000) / 1_000_000, h.closed, h.tracked));
    }
    // Held before the loss, still held within 250 ms of it, released after, and closed again on return.
    assert!(closed_at.iter().any(|&(t, c, tr)| t < 100 && c && tr));
    assert!(closed_at.iter().filter(|&&(t, _, _)| (100..340).contains(&t)).all(|&(_, c, _)| c));
    assert!(closed_at.iter().filter(|&&(t, _, _)| (360..500).contains(&t)).all(|&(_, c, tr)| !c && !tr));
    assert!(closed_at.iter().any(|&(t, c, tr)| t > 510 && c && tr));
    no_violations(&s);
}

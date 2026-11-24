//! `view_capture` on this machine (api.md 8, render.md 11): pe-api's capture port over pe-render's capture service.
//!
//! - [`RenderCaptures`] is the port pe-api calls: it puts each order on a [`CaptureQueue`] and waits for the shot.
//! - The queue is served by whichever loop draws here: the desktop window (`DesktopConfig.captures`), the headset
//!   ([`crate::vr::VrSink::with_captures`]), or, with neither, [`spawn_headless`]'s thread, which opens a GPU context
//!   only when the first order comes.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use pe_api::capture::{Aim, Captures, LookAt, Order, Refused, Shot};
use pe_render::capture_service::{self as cs, CaptureQueue, CaptureServer, ShotError};
use pe_render::gpu::{AdapterChoice, GpuContext};
use pe_render::present::FrameSlot;
use pe_render::scene::{RenderSettings, Renderer};

/// pe-api's capture port over the machine's capture queue.
pub struct RenderCaptures(pub Arc<CaptureQueue>);

impl Captures for RenderCaptures {
    fn capture(&self, order: Order, deadline: Instant) -> Result<Shot, Refused> {
        let aim = match order.aim {
            Aim::Player => cs::Aim::Player,
            Aim::Orbit { id, yaw_deg, pitch_deg, distance_m } => cs::Aim::Orbit { id, yaw_deg, pitch_deg, distance_m },
            Aim::Placed { at, look_at, fov_deg } => cs::Aim::Placed {
                at,
                look_at: match look_at {
                    LookAt::Object(id) => cs::LookAt::Object(id),
                    LookAt::Point(p) => cs::LookAt::Point(p),
                },
                fov_deg,
            },
        };
        let o = cs::Order {
            aim,
            size: order.size,
            highlight: order.highlight,
            labels: order.labels,
            visible: order.visible,
        };
        match self.0.request(o, deadline) {
            Ok(s) => Ok(Shot {
                png: s.png,
                frame: s.frame,
                eye: s.eye,
                target: s.target,
                fov_deg: s.fov_deg,
                visible: s.visible.into_iter().map(|v| (v.id, v.rect)).collect(),
            }),
            Err(ShotError::NoView) => Err(Refused::NoView),
            Err(ShotError::NotInView(id)) => Err(Refused::NotInView(id)),
            Err(ShotError::TimedOut) => Err(Refused::TimedOut),
            Err(ShotError::Unavailable(why)) => Err(Refused::Unavailable(why)),
            Err(ShotError::OutOfRange) => Err(Refused::OutOfRange),
            Err(ShotError::Failed(why)) => Err(Refused::Failed(why)),
        }
    }
}

/// How often the headless thread looks for an order, and polls an in-flight readback.
const IDLE_WAIT: Duration = Duration::from_millis(50);
const POLL: Duration = Duration::from_millis(2);

/// The headless capture thread: stop it with [`HeadlessCaptures::stop`] (or by dropping it), which also refuses
/// whatever is still waiting.
pub struct HeadlessCaptures {
    queue: Arc<CaptureQueue>,
    stop: Arc<AtomicBool>,
    /// What the server reported of each order (`CaptureServer::serve`), in order; no presenter counts them here.
    events: Arc<std::sync::Mutex<Vec<pe_render::counters::CaptureEvent>>>,
    thread: Option<JoinHandle<()>>,
}

impl HeadlessCaptures {
    pub fn stop(mut self) {
        self.halt();
    }

    /// The server's events so far, in order.
    pub fn events(&self) -> Vec<pe_render::counters::CaptureEvent> {
        self.events.lock().expect("the events").clone()
    }

    fn halt(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        self.queue.close();
    }
}

impl Drop for HeadlessCaptures {
    fn drop(&mut self) {
        self.halt();
    }
}

/// Serves `queue` from `slot`'s newest view on a thread of its own, for a machine with no window and no headset.
/// The GPU context (`choice`; tests take `SoftwareOnly`) is opened at the first order, so a machine nobody captures
/// on never opens one. With no adapter, each order is refused `Unavailable` and the next one tries again.
pub fn spawn_headless(
    queue: Arc<CaptureQueue>,
    slot: Arc<FrameSlot>,
    choice: AdapterChoice,
) -> std::io::Result<HeadlessCaptures> {
    let stop = Arc::new(AtomicBool::new(false));
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (q, s, ev) = (queue.clone(), stop.clone(), events.clone());
    let thread = thread::Builder::new().name("capture".into()).spawn(move || {
        let mut drawing: Option<(Renderer, CaptureServer)> = None;
        while !s.load(Ordering::SeqCst) {
            let busy = drawing.as_ref().is_some_and(|(_, server)| server.busy());
            if !busy && !q.wait_for_order(IDLE_WAIT) {
                continue;
            }
            if drawing.is_none() {
                match GpuContext::headless(choice) {
                    Ok(ctx) => {
                        drawing = Some((Renderer::new(ctx, RenderSettings::default()), CaptureServer::new(q.clone())))
                    }
                    Err(e) => {
                        q.refuse_waiting(ShotError::Unavailable(format!("No graphics adapter can draw here: {e:?}")));
                        continue;
                    }
                }
            }
            let Some(view) = slot.newest() else {
                thread::sleep(POLL);
                continue;
            };
            let (renderer, server) = drawing.as_mut().expect("opened above");
            let served = server.serve(renderer, &view, None);
            ev.lock().expect("the events").extend(served);
            thread::sleep(POLL);
        }
    })?;
    Ok(HeadlessCaptures { queue, stop, events, thread: Some(thread) })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use pe_render::gpu::{AdapterChoice, GpuContext};
    use pe_render::present::FrameSlot;
    use pe_seams::world_api::{Caller, CaptureRequest, ConnectionId, ErrorCode, Producer, ProducerKind, Scope};
    use pe_seams::{Frame, FrameClock, HostTime, SimRate};

    use super::*;
    use crate::api::{Facts, Ports};
    use crate::clock::LocalClock;
    use crate::sim::{Budget, Sim};
    use crate::standin::SoloAgreement;

    /// No hands: the capture test needs the world, not a player's input.
    struct NoHands;
    impl pe_seams::HandFeed for NoHands {
        fn hand_at(&self, _: pe_seams::Hand, _: HostTime) -> Option<pe_seams::HandInput> {
            None
        }
    }

    fn agent() -> Caller {
        Caller {
            connection: ConnectionId(1),
            principal: "agent".into(),
            member: crate::world::DEV_MEMBER.into(),
            scopes: [Scope::Read].into_iter().collect(),
            producer: Producer { kind: ProducerKind::Agent, harness: None, principal: None, requested_by: None },
            confirm: false,
        }
    }

    /// api.md 14's capture test: M1's ball pit captured headless through a software adapter (WARP on Windows,
    /// lavapipe on Linux) within the deadline, from an orbit around a ball, with `visible` naming the ball; the
    /// player's own view is refused, since a headless machine draws none. With no software adapter it reports
    /// itself skipped and passes, as pe-render's GPU tests do.
    #[test]
    fn the_ball_pit_is_captured_headless_through_a_software_adapter() {
        if let Err(e) = GpuContext::headless(AdapterChoice::SoftwareOnly) {
            eprintln!("skipped: no software adapter ({e:?})");
            return;
        }
        let rate = SimRate::DEFAULT;
        let clock: Arc<dyn FrameClock> = Arc::new(LocalClock::new(HostTime(0), rate));
        let me = crate::world::DEV_MEMBER;
        let runner = Box::new(
            pe_replica::logic::LogicRunner::new(pe_logic::Settings::default(), pe_logic::run::RunSettings::default())
                .expect("a runner"),
        );
        let replica = crate::world::open_ball_pit(7, me, rate, clock.clone(), runner).expect("the pit");
        let genesis = crate::api::snapshot_of(&*replica).expect("a pe-world replica");
        let facts = Facts { world_name: "Pit".into(), line: pe_seams::LineId::origin(), rate, me: Some(me) };
        let queue = CaptureQueue::new();
        let ports = Ports::new(genesis, facts, || {}).with_captures(Arc::new(RenderCaptures(queue.clone())));
        let slot = Arc::new(FrameSlot::new());
        let mut sim = Sim::new(
            Box::new(SoloAgreement::new(pe_seams::LineId::origin())),
            replica,
            clock.clone(),
            slot.clone(),
            Budget::new(rate),
        )
        .with_local(me, [7; 32], Arc::new(NoHands))
        .with_api(ports.clone());
        for f in 1..=3 {
            sim.tick(clock.host_time_of(Frame(f))).expect("ticked");
        }
        let headless = spawn_headless(queue, slot, AdapterChoice::SoftwareOnly).expect("the capture thread");
        let api = ports.api();
        let name = format!("{}0", pe_world::preset::names::BALL_PREFIX);
        let ball = format!("/{}/{name}", pe_world::preset::names::ROOT);
        let req = CaptureRequest {
            from: Some(pe_seams::world_api::From::Orbit {
                orbit: pe_seams::world_api::ObjRef(ball.clone()),
                yaw_deg: None,
                pitch_deg: None,
                distance_m: None,
            }),
            size: Some([320, 180]),
            highlight: vec![pe_seams::world_api::ObjRef(ball.clone())],
            label: true,
            visible_list: true,
            ..Default::default()
        };
        let started = Instant::now();
        let cap = api.capture(&agent(), &req, Instant::now() + Duration::from_secs(60)).expect("a capture");
        println!("captured {}x180 in {:?}, {} bytes of PNG", 320, started.elapsed(), cap.png.len());
        assert!(cap.png.starts_with(b"\x89PNG"), "a PNG");
        // For a reviewer's eyes: PE_CAPTURE_PNG=<path> writes the image out.
        if let Ok(path) = std::env::var("PE_CAPTURE_PNG") {
            std::fs::write(path, &cap.png).expect("the PNG written");
        }
        let visible = cap.visible.expect("visible, as asked");
        assert!(cap.visible_approx);
        assert!(visible.iter().any(|v| v.name == name), "{visible:?}");
        // A headless machine draws no view of its own.
        let e =
            api.capture(&agent(), &CaptureRequest::default(), Instant::now() + Duration::from_secs(10)).unwrap_err();
        assert_eq!(e.code, ErrorCode::CaptureUnavailable, "{e:?}");
        // What a loop's presenter would count (pull-ahead 1.3): both orders taken, the first drawn. The server answers
        // a refused order inside `serve` and the headless thread records the events only once `serve` returns, so the
        // refusal can reach this thread first (tier 2 run 45382142620, windows-x64): wait for the third event.
        use pe_render::counters::CaptureEvent::{Drawn, Queued};
        let until = Instant::now() + Duration::from_secs(10);
        let mut events = headless.events();
        while events.len() < 3 && Instant::now() < until {
            thread::sleep(Duration::from_millis(5));
            events = headless.events();
        }
        assert_eq!(events, [Queued, Drawn, Queued]);
        headless.stop();
    }
}

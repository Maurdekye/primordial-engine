//! The real-runtime smoke run (spec 10.3): open VR on Vulkan, draw `pe_xr::smoke::scene()` (a grey floor at y = 0,
//! a red ball ahead, a blue ball left, a green capsule right) plus a marker at each tracked hand or controller
//! (`pe_render::hands::markers`, left blue, right orange, at the late-latched pose) for a few seconds, leave VR, and
//! report what happened. It refuses to start without `PE_XR_REAL=1`, which needs the coordinator's go each time
//! (the user may be in VR on this PC).
//!
//! Run: `PE_XR_REAL=1 cargo run -p pe-xr --example xr_smoke -- [seconds]` (default 5).
//!
//! What it shows: the runtime loads, the headset is found, the GPU context is made through the runtime, the session
//! reaches FOCUSED and frames are drawn at the runtime's pace (pe-render's `render_stereo` into the swapchain, the
//! depth layer when the runtime takes it), and leaving VR tears down in OpenXR's order without an error.

use std::sync::Arc;
use std::time::{Duration, Instant};

use pe_render::DisplayHand;
use pe_render::hands;
use pe_render::scene::{RenderSettings, Renderer, Scene};
use pe_seams::{Pos, RigTransform};
use pe_xr::backend::DepthInfo;
use pe_xr::binding::{self, XrTargets};
use pe_xr::guard::Guard;
use pe_xr::hands::GraspConfig;
use pe_xr::loader::SystemDiscovery;
use pe_xr::open::{VrParts, open_vulkan};
use pe_xr::ring::{DEFAULT_WINDOW_NS, HandRing};
use pe_xr::session::{FrameRequest, FrameSink, Step};
use pe_xr::smoke;

/// Draws the smoke scene and the hand markers into the swapchain, and logs the first frame's eyes and hands.
struct Smoke {
    renderer: Renderer,
    targets: XrTargets,
    rig: RigTransform,
    scene: Scene,
    /// The smoke scene's own items; the hand markers follow them and are replaced every frame.
    base_items: usize,
    /// Drawn frames in which the left and right hand were tracked.
    hand_frames: [u64; 2],
    hand_logged: [bool; 2],
    released: u32,
    logged: bool,
}

impl FrameSink for Smoke {
    fn render(&mut self, frame: &FrameRequest<'_>) -> bool {
        let Some(target) = self.targets.stereo(frame.image, frame.depth_image, None) else {
            return false;
        };
        if !self.logged {
            self.logged = true;
            for (i, e) in frame.eyes.iter().enumerate() {
                println!(
                    "first drawn frame, eye {i}: position {:?} orientation {:?} fov {:?}",
                    e.pose.position, e.pose.orientation, e.fov
                );
            }
        }
        for (i, h) in frame.hands.iter().enumerate() {
            if let Some(p) = h {
                self.hand_frames[i] += 1;
                if !self.hand_logged[i] {
                    self.hand_logged[i] = true;
                    println!(
                        "first tracked hand {i} (0 left, 1 right): position {:?} orientation {:?}",
                        p.position, p.orientation
                    );
                }
            }
        }
        let shown = [0, 1].map(|i| frame.hands[i].map(|pose| DisplayHand { pose, joints: frame.hand_joints[i] }));
        self.scene.items.truncate(self.base_items);
        self.scene.items.extend(hands::markers(&shown, &self.rig));
        let cameras = binding::eye_cameras(&self.rig, frame.eyes);
        self.renderer.render_stereo(&self.scene, cameras, &target);
        true
    }

    fn depth(&self) -> Option<DepthInfo> {
        self.targets.has_depth().then(binding::vr_depth_info)
    }

    fn release_swapchain(&mut self) {
        self.released += 1;
        self.targets.release(self.renderer.context());
    }
}

fn main() {
    let seconds: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(5);
    let guard = Guard::from_env(false);
    if let Err(r) = guard.check() {
        eprintln!("xr_smoke: refused by the guard ({r:?}); set PE_XR_REAL=1, with the coordinator's go");
        std::process::exit(2);
    }
    let ring = Arc::new(HandRing::new(DEFAULT_WINDOW_NS, 8_333_333));
    let t0 = Instant::now();
    let vr = match open_vulkan(&SystemDiscovery, &guard, GraspConfig::default(), ring) {
        Ok(vr) => vr,
        Err(e) => {
            eprintln!("xr_smoke: VR not opened: {e}");
            std::process::exit(1);
        }
    };
    let backend = vr.session.backend();
    println!(
        "opened in {:?}; depth layer: {}; reference space: {}; hand pose: {:?}",
        t0.elapsed(),
        vr.targets.has_depth(),
        backend.reference_space,
        backend.pose_path
    );
    let VrParts { targets, mut session, gpu } = vr.into_parts();
    let scene = smoke::scene();
    let mut sink = Smoke {
        renderer: Renderer::new(gpu, RenderSettings::default()),
        targets,
        rig: RigTransform { pos: Pos::ORIGIN, rotation: [0.0, 0.0, 0.0, 1.0], scale: 1.0 },
        base_items: scene.items.len(),
        scene,
        hand_frames: [0; 2],
        hand_logged: [false; 2],
        released: 0,
        logged: false,
    };

    let run_for = Duration::from_secs(seconds);
    let mut exit_asked: Option<Instant> = None;
    let mut idle = 0u64;
    let end = loop {
        let now = Instant::now();
        if exit_asked.is_none() && now - t0 > run_for {
            exit_asked = Some(now);
            if let Err(e) = session.request_exit() {
                eprintln!("xr_smoke: exit request failed ({e:?}); the session ends anyway");
            }
        }
        if exit_asked.is_some_and(|t| now - t > Duration::from_secs(5)) {
            eprintln!("xr_smoke: no STOPPING within 5 s of the exit request; abandoning the session");
            session.abandon();
        }
        match session.step(&mut sink) {
            Step::Frame => {}
            Step::Idle => {
                idle += 1;
                std::thread::sleep(Duration::from_millis(100));
            }
            Step::Ended(why) => break why,
        }
    };

    let c = &session.counters;
    println!("ended: {end:?} after {:?}; last error: {:?}", t0.elapsed(), session.last_error);
    println!(
        "display frames {}, rendered {}, not rendered {}, invalid views {}, frame errors {}, degraded runs {}, \
         idle steps {idle}, swapchain released {}x",
        c.display_frames,
        c.frames_rendered,
        c.frames_not_rendered,
        c.invalid_view_frames,
        c.frame_errors,
        c.degraded_runs,
        sink.released
    );
    println!("hand tracked in drawn frames: left {}, right {}", sink.hand_frames[0], sink.hand_frames[1]);
    println!("{c:#?}");
    // OpenXR's order: the wrappers went at teardown (release_swapchain); now the session, then the GPU context.
    drop(session);
    drop(sink);
    println!("closed cleanly");
}

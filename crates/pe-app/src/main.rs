//! pe-app's binary (docs/spec/app.md). It runs a world alone: the simulation thread, with the stand-in agreement
//! (`pe_app::standin`) and pe-replica's replica over a new, empty pe-world world (`pe_app::world`), drawn in VR or in
//! the window on the main thread. The network, MCP, configuration and invites join as their crates land.
//!
//! `pe-app [--vr] [--seconds <n>]`. `--vr` turns VR on, as the configuration will (pe-xr's headset guard): VR opens
//! first, and pe-app goes to the desktop when VR does not open or when its session ends. `--seconds` ends the run
//! after `n` seconds, in VR or on the desktop; without it the window runs until it is closed.

use std::fmt::Debug;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pe_app::clock::{self, LocalClock};
use pe_app::sim::{self, Budget, Sim};
use pe_app::standin::SoloAgreement;
use pe_app::vr::{self, XrWait};
use pe_app::world;
use pe_render::desktop_hand::{DesktopHandFeed, DesktopHandSettings};
use pe_render::display::DisplaySettings;
use pe_render::effects::{EffectCommand, EffectSettings};
use pe_render::gpu::ContextError;
use pe_render::present::{FrameSlot, Presenter};
use pe_render::scene::{Environment, RenderSettings};
use pe_render::window::{DesktopConfig, DesktopReport, EffectSink, run_desktop};
use pe_seams::{FrameClock, LineId, RigPose, SimRate};
use pe_xr::guard::Guard;
use pe_xr::loader::SystemDiscovery;
use pe_xr::ring::{DEFAULT_WINDOW_NS, HandRing};

/// The simulation thread's stack until pe-logic's `Settings::thread_stack()` is on main (app.md 1): wasmtime's
/// default `max_wasm_stack` of 512 KiB plus 2 MiB, rounded up.
// TODO(pe-logic): spawn with `pe_logic::Settings::thread_stack()` (`max_wasm_stack` + 2 MiB, on ws/logic) once pe-logic
// lands, from the same `Settings` the replica runs guests with (design-reviewer's Low 1 on slice 1).
const SIM_STACK: usize = 4 << 20;

/// A new world's seed and cut cadence, until configuration names the world and its settings (app.md 4, step 1). The
/// stand-in agreement makes no cuts, so the cadence only places the replica's roots; 133 ms is pe-replica's test value.
const WORLD_SEED: u64 = 0;
const CUT_INTERVAL: Duration = Duration::from_millis(133);

/// The headset's display period until its frame loop reports the runtime's own (`HandRing::set_display_period`).
const VR_PERIOD_NS: i64 = 11_111_111;

/// M1 counts and logs effects; the sound library comes later (app.md 8).
struct LogEffects;

impl EffectSink for LogEffects {
    fn command(&mut self, c: &EffectCommand) {
        eprintln!("effect: {c:?}");
    }
}

/// The command line, until configuration lands (app.md 4 step 1).
struct Options {
    vr: bool,
    run_for: Option<Duration>,
}

fn options() -> Options {
    let mut o = Options { vr: false, run_for: None };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--vr" => o.vr = true,
            "--seconds" => {
                let s = args.next().and_then(|n| n.parse::<f64>().ok()).filter(|s| *s > 0.0);
                o.run_for = Some(s.and_then(|s| Duration::try_from_secs_f64(s).ok()).unwrap_or_else(|| usage()));
            }
            _ => usage(),
        }
    }
    o
}

fn presenter(rate: SimRate) -> Presenter {
    Presenter::new(DisplaySettings::default(), EffectSettings::default(), Environment::default(), rate.hz())
}

fn main() {
    let Options { vr, run_for } = options();
    let started = Instant::now();

    let rate = SimRate::DEFAULT;
    let host = clock::shared();
    let frames: Arc<dyn FrameClock> = Arc::new(LocalClock::new(host(), rate));
    let slot = Arc::new(FrameSlot::new());
    let replica = match world::open_new(WORLD_SEED, rate, CUT_INTERVAL, frames.clone()) {
        Ok(r) => r,
        Err(e) => fail(&format!("the replica did not start: {e}")),
    };
    let sim =
        Sim::new(Box::new(SoloAgreement::new(LineId::origin())), replica, frames, slot.clone(), Budget::new(rate));
    let sim = match sim::spawn(sim, host.clone(), SIM_STACK) {
        Ok(t) => t,
        Err(e) => fail(&format!("the simulation thread did not start: {e}")),
    };

    if vr {
        // The located hands, for the simulation thread's local input once pe-world's inputs land (app.md 3 step 2).
        let ring = Arc::new(HandRing::new(DEFAULT_WINDOW_NS, VR_PERIOD_NS));
        let guard = Guard::from_env(true);
        let effects = Box::new(LogEffects);
        match vr::start(
            &SystemDiscovery,
            &guard,
            ring,
            slot.clone(),
            presenter(rate),
            RenderSettings::default(),
            effects,
        ) {
            Ok(xr) => {
                // TODO(pe-render): in VR mode the main thread pumps a window's events and input, the desktop window
                // draws on the XR device while the session is idle or stopping (xr.md 3.1), and the XR thread draws
                // the desktop mirror (app.md 1, render.md 10). Until pe-render has that window the main thread waits
                // here; xr.md 3.3's retry after a lost session comes with it.
                let left = run_for.map_or(Duration::MAX, |d| d.saturating_sub(started.elapsed()));
                let waited = xr.wait(left);
                println!("vr: {waited:?}; at its end {:?}", xr.stop());
                if waited == XrWait::Running {
                    finish(sim.stop(), None);
                    return;
                }
                eprintln!("pe-app: the VR session ended; the desktop instead");
            }
            Err(why) => eprintln!("pe-app: {why}; the desktop instead"),
        }
    }

    let cfg = DesktopConfig {
        title: "Primordial Engine".into(),
        size: [1280, 720],
        present_mode: pe_render::wgpu::PresentMode::Fifo,
        vertical_fov: 1.0,
        clock: host,
        presenter: presenter(rate),
        render: RenderSettings::default(),
        hand: DesktopHandSettings::default(),
        eye: RigPose { position: [0.0, 1.4, 0.0], orientation: [0.0, 0.0, 0.0, 1.0] },
        // What is left of the run after VR, never zero, which the window would take as no time at all.
        run_for: run_for.map(|d| d.saturating_sub(started.elapsed()).max(Duration::from_millis(1))),
        snapshot: None,
    };
    // The window, its surface and its GPU context are made on this thread, one context at a time (app.md 4): the XR
    // thread has dropped its own context before `wait` answers.
    let window = run_desktop(cfg, slot, Arc::new(DesktopHandFeed::default()), Box::new(LogEffects));
    finish(sim.stop(), Some(window));
}

fn finish<S: Debug, E>(stats: Result<S, E>, window: Option<Result<DesktopReport, ContextError>>) {
    match window {
        Some(Ok(r)) => println!("window: {} frames on {}, {} skipped", r.frames, r.adapter, r.skipped),
        Some(Err(e)) => eprintln!("window: {e:?}"),
        None => {}
    }
    match stats {
        Ok(s) => println!("simulation: {s:?}"),
        Err(_) => fail("the simulation thread panicked"),
    }
}

fn usage() -> ! {
    fail("usage: pe-app [--vr] [--seconds <n>]")
}

fn fail(msg: &str) -> ! {
    eprintln!("pe-app: {msg}");
    std::process::exit(2)
}

//! pe-app's binary (docs/spec/app.md). It runs a world alone on the desktop: the simulation thread, with the stand-in
//! agreement (`pe_app::standin`) and pe-replica's replica over a new, empty pe-world world (`pe_app::world`), and the
//! window on the main thread. The network, VR, MCP, configuration and invites join as their crates land.
//!
//! `pe-app [--seconds <n>]` closes the window after `n` seconds; without it the window runs until it is closed.

use std::sync::Arc;
use std::time::Duration;

use pe_app::clock::{self, LocalClock};
use pe_app::sim::{self, Budget, Sim};
use pe_app::standin::SoloAgreement;
use pe_app::world;
use pe_render::desktop_hand::{DesktopHandFeed, DesktopHandSettings};
use pe_render::display::DisplaySettings;
use pe_render::effects::{EffectCommand, EffectSettings};
use pe_render::present::{FrameSlot, Presenter};
use pe_render::scene::{Environment, RenderSettings};
use pe_render::window::{DesktopConfig, EffectSink, run_desktop};
use pe_seams::{FrameClock, LineId, RigPose, SimRate};

/// The simulation thread's stack until pe-logic's `Settings::thread_stack()` is on main (app.md 1): wasmtime's
/// default `max_wasm_stack` of 512 KiB plus 2 MiB, rounded up.
// TODO(pe-logic): spawn with `pe_logic::Settings::thread_stack()` (`max_wasm_stack` + 2 MiB, on ws/logic) once pe-logic
// lands, from the same `Settings` the replica runs guests with (design-reviewer's Low 1 on slice 1).
const SIM_STACK: usize = 4 << 20;

/// A new world's seed and cut cadence, until configuration names the world and its settings (app.md 4, step 1). The
/// stand-in agreement makes no cuts, so the cadence only places the replica's roots; 133 ms is pe-replica's test value.
const WORLD_SEED: u64 = 0;
const CUT_INTERVAL: Duration = Duration::from_millis(133);

/// M1 counts and logs effects; the sound library comes later (app.md 8).
struct LogEffects;

impl EffectSink for LogEffects {
    fn command(&mut self, c: &EffectCommand) {
        eprintln!("effect: {c:?}");
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let run_for = match (args.next().as_deref(), args.next()) {
        (None, _) => None,
        (Some("--seconds"), Some(n)) => match n.parse::<f64>() {
            Ok(s) if s > 0.0 && s.is_finite() => Some(Duration::from_secs_f64(s)),
            _ => usage(),
        },
        _ => usage(),
    };

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

    let cfg = DesktopConfig {
        title: "Primordial Engine".into(),
        size: [1280, 720],
        present_mode: pe_render::wgpu::PresentMode::Fifo,
        vertical_fov: 1.0,
        clock: host,
        presenter: Presenter::new(
            DisplaySettings::default(),
            EffectSettings::default(),
            Environment::default(),
            rate.hz(),
        ),
        render: RenderSettings::default(),
        hand: DesktopHandSettings::default(),
        eye: RigPose { position: [0.0, 1.4, 0.0], orientation: [0.0, 0.0, 0.0, 1.0] },
        run_for,
        snapshot: None,
    };
    // The window, its surface and its GPU context are made on this thread, one context at a time (app.md 4).
    let window = run_desktop(cfg, slot, Arc::new(DesktopHandFeed::default()), Box::new(LogEffects));
    let stats = sim.stop();
    match window {
        Ok(r) => println!("window: {} frames on {}, {} skipped", r.frames, r.adapter, r.skipped),
        Err(e) => eprintln!("window: {e:?}"),
    }
    match stats {
        Ok(s) => println!("simulation: {s:?}"),
        Err(_) => fail("the simulation thread panicked"),
    }
}

fn usage() -> ! {
    fail("usage: pe-app [--seconds <n>]")
}

fn fail(msg: &str) -> ! {
    eprintln!("pe-app: {msg}");
    std::process::exit(2)
}

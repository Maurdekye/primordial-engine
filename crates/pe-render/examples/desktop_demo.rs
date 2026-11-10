//! The desktop window against a stand-in simulation: balls bouncing on a floor, published at 60 Hz from their own
//! thread into the frame-view slot, with one deliberate 30 cm correction every 2 s so the cue shows.
//! `cargo run -p pe-render --example desktop_demo -- [seconds]`. Draws on the GPU: run it only with a go.
//! The stand-in is not the engine's simulation; it exercises the window, pacing, the display layer and the hand.

use std::sync::Arc;
use std::time::{Duration, Instant};

use pe_render::desktop_hand::{DesktopHandFeed, DesktopHandSettings};
use pe_render::display::DisplaySettings;
use pe_render::effects::{EffectCommand, EffectSettings};
use pe_render::present::{FrameSlot, Presenter};
use pe_render::scene::{Environment, RenderSettings};
use pe_render::window::{DesktopConfig, EffectSink, run_desktop};
use pe_seams::{
    Appearance, DomainId, Frame, FrameView, HostTime, ObjectClass, ObjectId, Offset, Pos, RigPose, RigTransform, Shape,
    ViewCounters, ViewFlags, ViewObject,
};

struct Print;

impl EffectSink for Print {
    fn command(&mut self, c: &EffectCommand) {
        eprintln!("effect: {c:?}");
    }
}

fn object(id: u128, p: [f64; 3], v: [f32; 3], shape: Shape, color: [f32; 4], radius: f32) -> ViewObject {
    ViewObject {
        id: ObjectId(id),
        class: ObjectClass::World,
        pos: Pos::ORIGIN.checked_add(Offset(p)).unwrap(),
        rotation: [0.0, 0.0, 0.0, 1.0],
        scale: [1.0; 3],
        linear_velocity: v,
        angular_velocity: [0.0; 3],
        shape: Some(shape),
        appearance: Some(Appearance {
            base_color: color,
            emissive: [0.0; 3],
            roughness: 0.5,
            metalness: 0.0,
            material: None,
        }),
        light: None,
        radius,
        hand_of: None,
        held_by: None,
        flags: ViewFlags::default(),
        version: None,
    }
}

fn main() {
    let secs: f64 = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(10.0);
    let start = Instant::now();
    let clock: Arc<dyn Fn() -> HostTime + Send + Sync> = Arc::new(move || HostTime(start.elapsed().as_nanos() as i64));
    let slot = Arc::new(FrameSlot::new());
    let publisher = slot.clone();
    let c2 = clock.clone();
    std::thread::spawn(move || {
        let hz = 60u64;
        for f in 0u64.. {
            let t = f as f64 / hz as f64;
            let mut objects = vec![object(
                1,
                [0.0, -0.05, -3.0],
                [0.0; 3],
                Shape::Box { half_extents: [4.0, 0.05, 4.0] },
                [0.5, 0.5, 0.55, 1.0],
                5.7,
            )];
            for i in 0..12u128 {
                let phase = i as f64 * 0.37;
                let period = 0.9 + 0.05 * i as f64;
                let u = ((t + phase) % period) / period;
                // A bounce: height 1 - (2u-1)^2, with its velocity.
                let h = 0.15 + 0.8 * (1.0 - (2.0 * u - 1.0).powi(2));
                let vy = -0.8 * 2.0 * (2.0 * u - 1.0) * 2.0 / period;
                let mut x = -1.2 + 0.22 * i as f64;
                // The deliberate correction: every 2 s, ball 5 is moved 30 cm for one second.
                if i == 5 && (t % 4.0) >= 2.0 {
                    x += 0.3;
                }
                let mut o = object(
                    10 + i,
                    [x, h, -3.0],
                    [0.0, vy as f32, 0.0],
                    Shape::Sphere { radius: 0.1 },
                    [0.85, 0.3 + 0.05 * i as f32, 0.2, 1.0],
                    0.1,
                );
                if i == 5 && ((t % 4.0) - 2.0).abs() < 0.5 / hz as f64
                    || (i == 5 && (t % 4.0) < 0.5 / hz as f64 && f > 0)
                {
                    o.flags = o.flags.with(ViewFlags::CORRECTED);
                }
                objects.push(o);
            }
            objects.sort_by_key(|o| o.id);
            let due = HostTime((f as i128 * 1_000_000_000 / hz as i128) as i64);
            publisher.publish(Arc::new(FrameView {
                frame: Frame(f),
                domain: DomainId(0),
                sim_time: due,
                final_frame: None,
                local_rig: RigTransform { pos: Pos::ORIGIN, rotation: [0.0, 0.0, 0.0, 1.0], scale: 1.0 },
                objects,
                effects: vec![],
                retracted: vec![],
                counters: ViewCounters::default(),
                changes: vec![],
            }));
            let next = HostTime(((f + 1) as i128 * 1_000_000_000 / hz as i128) as i64);
            let wait = next.0 - c2().0;
            if wait > 0 {
                std::thread::sleep(Duration::from_nanos(wait as u64));
            }
        }
    });
    let cfg = DesktopConfig {
        title: "pe-render desktop demo (stand-in simulation)".into(),
        size: [1280, 720],
        present_mode: pe_render::wgpu::PresentMode::Fifo,
        vertical_fov: 1.0,
        clock,
        presenter: Presenter::new(DisplaySettings::default(), EffectSettings::default(), Environment::default(), 60),
        render: RenderSettings::default(),
        hand: DesktopHandSettings::default(),
        eye: RigPose { position: [0.0, 1.4, 0.0], orientation: [-0.199, 0.0, 0.0, 0.980] },
        run_for: Some(Duration::from_secs_f64(secs)),
    };
    let r = run_desktop(cfg, slot, Arc::new(DesktopHandFeed::default()), Box::new(Print)).expect("desktop");
    let mut iv = r.intervals_ms.clone();
    iv.sort_by(f64::total_cmp);
    let pct = |p: f64| iv.get(((iv.len().max(1) - 1) as f64 * p) as usize).copied().unwrap_or(0.0);
    println!(
        "adapter {}; monitor {:?} mHz; frames {}; skipped {}; quality changes {}",
        r.adapter, r.monitor_mhz, r.frames, r.skipped, r.quality_changes
    );
    println!("present interval p50 {:.2} ms, p99 {:.2} ms", pct(0.5), pct(0.99));
}

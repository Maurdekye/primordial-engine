//! Draw the plane preset's movement test course (pe-world `preset::course`) with the engine's own renderer, on the
//! software adapter, from a few cameras, and write each frame read back to a PNG: the readback for the course's
//! review (never a screen capture). `cargo run -p pe-app --release --example course_png -- <out dir>`.
//!
//! The world is the plane with two members, stepped a second on pe-replica so the view is what a session draws.

use std::sync::Arc;
use std::time::Duration;

use pe_render::camera::{Camera, DepthRange, Fov};
use pe_render::gpu::{AdapterChoice, GpuContext};
use pe_render::pose::WorldPose;
use pe_render::scene::{RenderSettings, Renderer, Scene};
use pe_replica::mock::MockClock;
use pe_replica::{PeWorld, ReplicaConfig, ReplicaImpl};
use pe_seams::test_util::TestKeys;
use pe_seams::{DomainId, EntrySource, FinalCut, Frame, Hash, HostTime, LineId, Offset, Pos, Replica, SignedEntry};
use pe_world::preset::{Preset, Session, m1_session};
use pe_world::{BlobStore, perm};

struct Empty;

impl EntrySource for Empty {
    fn final_cut(&self) -> Option<&FinalCut> {
        None
    }
    fn entries_at(&self, _frame: Frame) -> Option<&[SignedEntry]> {
        Some(&[])
    }
    fn kept_from(&self) -> Frame {
        Frame(0)
    }
    fn keep_final_from(&mut self, _cv: Frame, _floor: Frame) {}
    fn take_changed_from(&mut self) -> Option<Frame> {
        None
    }
}

fn at(d: [f64; 3]) -> Pos {
    Pos::ORIGIN.checked_add(Offset(d)).expect("in range")
}

/// A yaw about +y then a pitch down about the turned x axis (drawing only, so the standard library's trig).
fn look(p: [f64; 3], yaw: f64, pitch: f64) -> WorldPose {
    let (sy, cy) = (yaw / 2.0).sin_cos();
    let (sp, cp) = (-pitch / 2.0).sin_cos();
    let rot = pe_render::pose::quat([(cy * sp) as f32, (sy * cp) as f32, (-sy * sp) as f32, (cy * cp) as f32]);
    WorldPose { pos: at(p), rot }
}

fn main() {
    let out = std::path::PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".into()));
    let keys = TestKeys::new(3, 2);
    let s = Session {
        seed: 5,
        origin: Pos::ORIGIN,
        preset: Preset::Plane,
        line: LineId(Hash([7; 32])),
        rate: pe_seams::SimRate::DEFAULT,
        gravity: pe_world::GRAVITY,
        agree: b"pe-agree's settings".to_vec(),
        members: vec![(keys.key(0), perm::ADMIN | perm::EDIT), (keys.key(1), perm::EDIT)],
    };
    let world = m1_session(&s, BlobStore::default()).expect("the plane");
    let clock = MockClock { rate: pe_seams::SimRate::DEFAULT, start: HostTime(1_000_000_000) };
    let config = ReplicaConfig::new(pe_seams::SimRate::DEFAULT, Duration::from_millis(133), Arc::new(clock));
    let mut r = ReplicaImpl::new(PeWorld::new(world, DomainId(0)), config).expect("a replica");
    r.advance(Frame(60), &mut Empty);
    let view = r.view();
    let scene = Scene::simulated(&view);
    eprintln!("{} drawn items, {} lights", scene.items.len(), scene.lights.len());
    let ctx = GpuContext::headless(AdapterChoice::SoftwareOnly).expect("software adapter");
    eprintln!("adapter: {}", ctx.describe());
    let mut renderer = Renderer::new(ctx, RenderSettings::default());
    // (name, camera position, yaw about +y, pitch down), radians; a yaw of 0 looks along -z.
    let shots: [(&str, [f64; 3], f64, f64); 5] = [
        ("overview-south", [0.0, 34.0, 44.0], 0.0, 0.72),
        ("overview-north", [0.0, 34.0, -48.0], std::f64::consts::PI, 0.72),
        ("halfpipe-basin", [0.0, 9.0, -4.0], 0.0, 0.45),
        ("slopes-stairs", [0.0, 6.0, 12.0], 0.0, 0.25),
        ("south-stations", [4.0, 9.0, -2.0], std::f64::consts::PI, 0.5),
    ];
    let size = [1280, 720];
    for (name, p, yaw, pitch) in shots {
        let cam = Camera::placed(look(p, yaw, pitch), Fov::symmetric(1.1, 16.0 / 9.0), DepthRange::DESKTOP);
        let img = renderer
            .capture(&scene, cam, size)
            .expect("capture")
            .wait(&renderer.context().device, Duration::from_secs(120))
            .expect("readback");
        let path = out.join(format!("course-{name}.png"));
        std::fs::write(&path, img.encode_png()).expect("write");
        eprintln!("wrote {}", path.display());
    }
}

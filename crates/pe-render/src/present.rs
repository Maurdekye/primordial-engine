//! The render thread's per-frame glue (render.md 3): the frame-view slot the simulation publishes into, and the
//! presenter that turns the newest view into a drawn scene and effect commands.
//!
//! - **The slot** is one atomic pointer swap (`arc-swap`): the simulation stores an `Arc<FrameView>`, the render
//!   thread loads the newest. Neither ever waits on the other, and the simulation keeps stepping while nothing draws.
//! - **The presenter** runs the display layer (D1-D5, D7) and the effect gate (D6) once per display frame, and builds
//!   the [`Scene`] every view of that frame draws.

use std::sync::Arc;

use arc_swap::ArcSwapOption;
use pe_seams::FrameView;

use crate::LocalPresentation;
use crate::camera::Camera;
use crate::display::{DisplayFrame, DisplayLayer, DisplaySettings, FrameInput};
use crate::effects::{EffectCommand, EffectGate, EffectSettings};
use crate::scene::{Environment, Scene};

/// Where the simulation publishes frame views for the render thread.
#[derive(Default)]
pub struct FrameSlot {
    newest: ArcSwapOption<FrameView>,
}

impl FrameSlot {
    pub fn new() -> FrameSlot {
        FrameSlot::default()
    }

    /// Publish a view (the simulation thread). The view is never changed afterwards.
    pub fn publish(&self, view: Arc<FrameView>) {
        self.newest.store(Some(view));
    }

    /// The newest view (the render thread), or `None` before the first.
    pub fn newest(&self) -> Option<Arc<FrameView>> {
        self.newest.load_full()
    }
}

/// One display frame, ready to draw.
pub struct PresentedFrame {
    pub scene: Scene,
    pub display: DisplayFrame,
    /// Plays and stops for the sinks: sounds to pe-app, haptics to pe-xr; particles are pe-render's.
    pub effects: Vec<EffectCommand>,
}

/// The display layer and the effect gate, on the render thread. Survives a lost GPU context.
pub struct Presenter {
    display: DisplayLayer,
    effects: EffectGate,
    environment: Environment,
    sim_rate_hz: u32,
    last: Option<Arc<FrameView>>,
}

impl Presenter {
    /// `sim_rate_hz` is the world's simulation rate (a world setting), used to place effects' frames in time.
    pub fn new(
        display: DisplaySettings,
        effects: EffectSettings,
        environment: Environment,
        sim_rate_hz: u32,
    ) -> Presenter {
        Presenter {
            display: DisplayLayer::new(display),
            effects: EffectGate::new(effects),
            environment,
            sim_rate_hz,
            last: None,
        }
    }

    pub fn display(&self) -> &DisplayLayer {
        &self.display
    }

    pub fn display_mut(&mut self) -> &mut DisplayLayer {
        &mut self.display
    }

    pub fn effects(&self) -> &EffectGate {
        &self.effects
    }

    /// One display frame against `view`, seen through `cameras` (both eyes, or the window).
    pub fn frame(&mut self, view: &Arc<FrameView>, local: &LocalPresentation, cameras: &[Camera]) -> PresentedFrame {
        let volumes: Vec<_> = cameras.iter().map(Camera::volume).collect();
        let display =
            self.display.frame(FrameInput { view, display_time: local.display_time, volumes: &volumes, local });
        // `take` also releases what is due; between arrivals, only the waits run.
        let effects = if self.last.as_ref().is_none_or(|l| !Arc::ptr_eq(l, view)) {
            self.last = Some(view.clone());
            self.effects.take(view, self.sim_rate_hz, local.display_time)
        } else {
            self.effects.due(local.display_time)
        };
        let mut scene = Scene::displayed(view, &display);
        scene.environment = self.environment;
        PresentedFrame { scene, display, effects }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pe_seams::{DomainId, Frame, HostTime, Pos, RigTransform, ViewCounters};

    fn view(f: u64) -> Arc<FrameView> {
        Arc::new(FrameView {
            frame: Frame(f),
            domain: DomainId(0),
            sim_time: HostTime::from_millis(f as i64 * 16),
            final_frame: None,
            local_rig: RigTransform { pos: Pos::ORIGIN, rotation: [0.0, 0.0, 0.0, 1.0], scale: 1.0 },
            objects: vec![],
            effects: vec![],
            retracted: vec![],
            counters: ViewCounters::default(),
            changes: vec![],
        })
    }

    #[test]
    fn the_slot_gives_the_newest_view() {
        let s = FrameSlot::new();
        assert!(s.newest().is_none());
        s.publish(view(1));
        s.publish(view(2));
        assert_eq!(s.newest().unwrap().frame, Frame(2));
    }

    #[test]
    fn a_view_is_taken_once_however_many_display_frames_draw_it() {
        let mut p = Presenter::new(DisplaySettings::default(), EffectSettings::default(), Environment::default(), 60);
        let v = view(3);
        let local = LocalPresentation { display_time: HostTime::from_millis(50), hands: [None, None] };
        for _ in 0..3 {
            p.frame(&v, &local, &[]);
        }
        assert_eq!(p.display().counters().views_taken, 1);
        assert_eq!(p.display().counters().display_frames, 3);
    }
}

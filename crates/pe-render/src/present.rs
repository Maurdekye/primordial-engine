//! The render thread's per-frame glue (render.md 3): the frame-view slot the simulation publishes into, and the
//! presenter that turns the newest view into a drawn scene and effect commands.
//!
//! - **The slot** is one atomic pointer swap (`arc-swap`): the simulation stores an `Arc<FrameView>`, the render
//!   thread loads the newest. Neither ever waits on the other, and the simulation keeps stepping while nothing draws.
//! - **The presenter** runs the display layer (D1-D5, D7) and the effect gate (D6) once per display frame, and builds
//!   the [`Scene`] every view of that frame draws.
//! - **The counters** (render.md 13): the presenter takes the display layer's counters every frame into its
//!   [`RenderCounters`], so `DisplayLayer::counters` reads only the current frame's; the caller reports each draw
//!   ([`Presenter::drawn`]) and each capture event, and pe-app reads [`Presenter::counters`].
//! - **The notices** (render.md 8.1): with [`Presenter::set_notices`], each new view's `changes` feed them, pe-app
//!   hands in agreement events ([`Presenter::agreement`]), and each [`PresentedFrame`] carries the lines to show.
//!   The caller turns them into `Scene::overlay` (`notices::StripCache`, then `window_overlay` or `Panel::place`).

use std::sync::Arc;

use arc_swap::ArcSwapOption;
use pe_seams::{AgreeEvent, FrameView, HostTime};

use crate::LocalPresentation;
use crate::camera::Camera;
use crate::counters::{CaptureEvent, RenderCounters};
use crate::display::{DisplayFrame, DisplayLayer, DisplaySettings, FrameInput};
use crate::effects::{EffectCommand, EffectGate, EffectSettings};
use crate::notices::{Line, Notices};
use crate::pacing::Quality;
use crate::scene::{Environment, FrameStats, Scene};

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
    /// The notices to show this frame, oldest first (empty without [`Presenter::set_notices`]).
    pub notices: Vec<Line>,
}

/// The display layer and the effect gate, on the render thread. Survives a lost GPU context.
pub struct Presenter {
    display: DisplayLayer,
    effects: EffectGate,
    environment: Environment,
    sim_rate_hz: u32,
    last: Option<Arc<FrameView>>,
    counters: RenderCounters,
    notices: Option<Notices>,
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
            counters: RenderCounters::new(),
            notices: None,
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

    /// pe-render's counters, per second and over the run (render.md 13).
    pub fn counters(&self) -> &RenderCounters {
        &self.counters
    }

    /// Report a draw of the last presented frame, at `at` (host time), at `quality`. Once per presented frame: a
    /// stereo frame is one draw, not one per eye.
    pub fn drawn(&mut self, at: pe_seams::HostTime, stats: &FrameStats, quality: Quality) {
        self.counters.drawn(at, stats, quality);
    }

    /// Tell the local player's notices from here on (render.md 8.1).
    pub fn set_notices(&mut self, notices: Notices) {
        self.notices = Some(notices);
    }

    pub fn notices_mut(&mut self) -> Option<&mut Notices> {
        self.notices.as_mut()
    }

    /// An agreement event from pe-app: voids of the local member's entries become notices.
    pub fn agreement(&mut self, e: &AgreeEvent, now: HostTime) {
        if let Some(n) = &mut self.notices {
            n.agreement(e, now);
        }
    }

    /// Report a capture's progress (the capture port, render.md 11).
    pub fn capture_event(&mut self, at: pe_seams::HostTime, e: CaptureEvent) {
        self.counters.capture(at, e);
    }

    /// One display frame against `view`, seen through `cameras` (both eyes, or the window).
    pub fn frame(&mut self, view: &Arc<FrameView>, local: &LocalPresentation, cameras: &[Camera]) -> PresentedFrame {
        let volumes: Vec<_> = cameras.iter().map(Camera::volume).collect();
        let display =
            self.display.frame(FrameInput { view, display_time: local.display_time, volumes: &volumes, local });
        let taken = self.display.take_counters();
        self.counters.display_frame(local.display_time, &taken, &display, local.display_time);
        // `take` also releases what is due; between arrivals, only the waits run.
        let effects = if self.last.as_ref().is_none_or(|l| !Arc::ptr_eq(l, view)) {
            self.last = Some(view.clone());
            if let Some(n) = &mut self.notices {
                n.observe(view, local.display_time);
            }
            self.effects.take(view, self.sim_rate_hz, local.display_time)
        } else {
            self.effects.due(local.display_time)
        };
        let notices = self.notices.as_mut().map(|n| n.lines(local.display_time)).unwrap_or_default();
        let mut scene = Scene::displayed(view, &display);
        scene.environment = self.environment;
        PresentedFrame { scene, display, effects, notices }
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
        let run = p.counters().run();
        assert_eq!(run.display.views_taken, 1);
        assert_eq!(run.display.display_frames, 3);
        assert_eq!(run.view_age_ms.count, 3);
    }

    /// Notices: each new view's `changes` feed them once, and every presented frame carries the lines.
    #[test]
    fn presented_frames_carry_the_notices() {
        use crate::notices::NoticeSettings;
        use pe_seams::{Class, EntryRef, Hash, MemberKey, Outcome, Reason, StepRecord};
        let me = MemberKey([1; 32]);
        let mut p = Presenter::new(DisplaySettings::default(), EffectSettings::default(), Environment::default(), 60);
        let local = LocalPresentation { display_time: HostTime::from_millis(50), hands: [None, None] };
        assert!(p.frame(&view(1), &local, &[]).notices.is_empty(), "no notices set");
        p.set_notices(Notices::new(NoticeSettings::default(), me, 60));
        let mut v = (*view(4)).clone();
        v.final_frame = Some(Frame(4));
        v.changes = vec![StepRecord {
            entry: EntryRef(Hash([9; 32])),
            author: me,
            class: Class::Input,
            frame: Frame(3),
            outcome: Outcome::Refused { op: None, reason: Reason::OutOfReach },
        }];
        let v = Arc::new(v);
        for _ in 0..3 {
            let f = p.frame(&v, &local, &[]);
            assert_eq!(f.notices.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), ["Out of reach."]);
        }
        assert_eq!(p.notices_mut().unwrap().told_total(), 1, "the view was read once");
    }
}

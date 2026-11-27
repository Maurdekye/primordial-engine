//! The render thread's per-frame glue (render.md 3): the frame-view slot the simulation publishes into, and the
//! presenter that turns the newest view into a drawn scene and effect commands.
//!
//! - **The slot** keeps every view published since the reader last took, in order, each numbered, as a [`Batch`]
//!   (render.md 3; @coordinator-opus's ruling). A view's `CORRECTED` flags, `changes` and `retracted`
//!   hold since the last publication, so a reader that took only the newest would lose those of the views between
//!   (found by render.md 14 test 6). It keeps [`SLOT_SPAN`] (about 250 ms) of simulated time; past that the oldest go,
//!   and the reader sees the hole in the numbers as a gap. The simulation never waits on the render thread: both hold
//!   the slot's lock only to push or to drain.
//! - **The presenter** runs the display layer (D1-D5, D7) and the effect gate (D6) once per display frame, and builds
//!   the [`Scene`] every view of that frame draws.
//! - **The counters** (render.md 13): the presenter takes the display layer's counters every frame into its
//!   [`RenderCounters`], so `DisplayLayer::counters` reads only the current frame's; the caller reports each draw
//!   ([`Presenter::drawn`]) and each capture event, and pe-app reads [`Presenter::counters`].
//! - **The notices** (render.md 8.1): with [`Presenter::set_notices`], each new view's `changes` feed them, pe-app
//!   hands in agreement events ([`Presenter::agreement`]), and each [`PresentedFrame`] carries the lines to show.
//!   The caller turns them into `Scene::overlay` (`notices::StripCache`, then `window_overlay` or `Panel::place`).
//! - **The status line** ([`Presenter::set_status`]): a line pe-app keeps up while a state lasts ("Joining: catching
//!   up…" while a joiner simulates on to the present), carried as [`PresentedFrame::status`] for the caller to show
//!   above the notices.
//! - **Worlds** (pe-app's in-game Join and New session; app.md 4.1): one slot and one presenter serve every session a
//!   process runs. [`FrameSlot::begin_world`] starts the next world once the last one's simulation has stopped, and
//!   each [`Batch`] says its world. The presenter starts over at the first batch of a new world that holds a view
//!   ([`Presenter::frame_batch`]): nothing of the old world is blended into the new one, its effects stop, and it isn't
//!   counted as a gap. Until then it draws the old world's last view again, and counts those frames as held
//!   (`RunReport::held_for_swap`), not as display frames.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arc_swap::ArcSwapOption;
use pe_seams::{AgreeEvent, FrameView, HostTime, MoveInput, RigTransform};

use crate::LocalPresentation;
use crate::camera::Camera;
use crate::counters::{CaptureEvent, RenderCounters, RunReport};
use crate::display::{DisplayFrame, DisplayLayer, DisplaySettings, FrameInput, OwnHands, OwnHandsSwitch};
use crate::drawn_time::DrawnTime;
use crate::effects::{EffectCommand, EffectGate, EffectSettings};
use crate::notices::{Line, Notices, StatusLine};
use crate::pacing::Quality;
use crate::rig::RigTrack;
use crate::scene::{Environment, FrameStats, Scene};

/// How much simulated time the slot keeps for a reader that hasn't taken: about 250 ms of publications at the
/// world's rate (8 at 30 Hz, 30 at 120 Hz).
pub const SLOT_SPAN: Duration = Duration::from_millis(250);
/// A cap on the count as well, for a replica that re-publishes one frame many times.
const SLOT_MAX: usize = 512;

/// Where the simulation publishes frame views for the render thread.
#[derive(Default)]
pub struct FrameSlot {
    newest: ArcSwapOption<FrameView>,
    queue: Mutex<SlotQueue>,
}

#[derive(Default)]
struct SlotQueue {
    views: VecDeque<Arc<FrameView>>,
    /// The number the next publication gets; the queue's views are the ones just before it.
    next: u64,
    /// The world publications go to now, from 0, and its start ([`FrameSlot::begin_world`]).
    world: u64,
    start: Option<WorldStart>,
    /// The world of `FrameSlot::newest`.
    newest_world: u64,
}

/// What the presenter takes from a new world (pe-app's next session): `Shown`'s delay is the session's, the opener's
/// choice in its genesis record (app.md 3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorldStart {
    pub display_delay: Duration,
}

/// The views published since the reader last took, oldest first.
#[derive(Clone, Debug, Default)]
pub struct Batch {
    /// The number of `views[0]`. Each publication gets the next number, from 0.
    pub first: u64,
    pub views: Vec<Arc<FrameView>>,
    /// The world the slot publishes to now, which every view here belongs to, and its start (`None` for world 0
    /// unless [`FrameSlot::begin_world`] gave one).
    pub world: u64,
    pub start: Option<WorldStart>,
}

impl Batch {
    /// The number the batch after this one starts at, if nothing is lost between.
    pub fn end(&self) -> u64 {
        self.first + self.views.len() as u64
    }
}

impl FrameSlot {
    pub fn new() -> FrameSlot {
        FrameSlot::default()
    }

    /// Publish a view (the simulation thread). The view is never changed afterwards. Views older than [`SLOT_SPAN`]
    /// behind it, by simulated time, go unread.
    pub fn publish(&self, view: Arc<FrameView>) {
        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        // Under the queue's lock, so `newest_current` never pairs this view with the world before it.
        self.newest.store(Some(view.clone()));
        q.newest_world = q.world;
        let newest = view.sim_time;
        q.views.push_back(view);
        q.next += 1;
        let span = SLOT_SPAN.as_nanos() as i64;
        while q.views.len() > SLOT_MAX || q.views.front().is_some_and(|v| newest.0 - v.sim_time.0 > span) {
            q.views.pop_front();
        }
    }

    /// Every view published since the last take (the render thread, once per display frame). A reader that draws
    /// takes batches; one that took only [`FrameSlot::newest`] would lose the flags, changes and retractions of the
    /// views between.
    pub fn take(&self) -> Batch {
        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        let views: Vec<Arc<FrameView>> = q.views.drain(..).collect();
        Batch { first: q.next - views.len() as u64, views, world: q.world, start: q.start }
    }

    /// Starts the next world (pe-app, between sessions, once the last session's simulation thread has ended, so no
    /// view of the old world comes after): the views not yet taken are dropped, and the next publications are the new
    /// world's. `newest` is kept, so a reader that waits for a first view (the desktop window) goes on drawing the old
    /// world's last one until the new world publishes. Holds the lock as briefly as a publish does. The new world's
    /// number.
    pub fn begin_world(&self, start: WorldStart) -> u64 {
        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        q.views.clear();
        q.world += 1;
        q.start = Some(start);
        q.world
    }

    /// The world publications go to now.
    pub fn world(&self) -> u64 {
        self.queue.lock().unwrap_or_else(|e| e.into_inner()).world
    }

    /// The newest view if it belongs to the current world: `None` between [`FrameSlot::begin_world`] and the new
    /// world's first view, so a reader that aims at objects (scripted hands) or captures them (a headless capture)
    /// never takes the old world for the new one.
    pub fn newest_current(&self) -> Option<Arc<FrameView>> {
        let q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        if q.newest_world != q.world {
            return None;
        }
        self.newest.load_full()
    }

    /// The newest view (the render thread), or `None` before the first.
    /// The newest view, for "has anything been published?" and for tests. It doesn't take: the next batch still
    /// holds it.
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
    /// The status line's text this frame, if one is up ([`Presenter::set_status`]).
    pub status: Option<String>,
}

/// Local trial observer of the finished display poses, newest publication,
/// publications just taken for this display frame, whether earlier ones were lost, and the active camera(s).
#[cfg(feature = "audio-trial")]
pub type AudioObserver = Box<dyn FnMut(&DisplayFrame, &FrameView, &[Arc<FrameView>], bool, &[Camera]) + Send>;

/// The display layer and the effect gate, on the render thread. Survives a lost GPU context.
pub struct Presenter {
    display: DisplayLayer,
    effects: EffectGate,
    environment: Environment,
    sim_rate_hz: u32,
    last: Option<Arc<FrameView>>,
    /// The number the next batch should start at (none before the first batch).
    next: Option<u64>,
    gaps: u64,
    counters: RenderCounters,
    notices: Option<Notices>,
    status: Option<Arc<StatusLine>>,
    /// Where the own hands are drawn, as another thread switches it ([`Presenter::set_own_hands_switch`]).
    own_hands: Option<Arc<OwnHandsSwitch>>,
    /// At the last switch: the mode it ended, and the run's counters until then ([`Presenter::take_own_hands_window`]).
    own_hands_ended: Option<(OwnHands, RunReport)>,
    /// The local rig of the views taken, and the one [`Presenter::drawn_rig`] resolved for the frame about to be drawn
    /// (crate::rig; locomotion L4c).
    rig: RigTrack,
    drawn: Option<RigTransform>,
    /// The world drawn ([`Batch::world`]), the `Stop`s the last world change owes the effect sinks, and whether the
    /// audio observer still has to hear of it (as a gap, so it drops its history).
    world: u64,
    world_stops: Vec<EffectCommand>,
    world_changed: bool,
    #[cfg(feature = "audio-trial")]
    audio_observer: Option<AudioObserver>,
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
            next: None,
            gaps: 0,
            counters: RenderCounters::new(),
            notices: None,
            status: None,
            own_hands: None,
            own_hands_ended: None,
            rig: RigTrack::default(),
            drawn: None,
            world: 0,
            world_stops: Vec::new(),
            world_changed: false,
            #[cfg(feature = "audio-trial")]
            audio_observer: None,
        }
    }

    /// The world drawn now ([`Batch::world`]).
    pub fn world(&self) -> u64 {
        self.world
    }

    /// At the first batch of a new world that holds a view, start over: a new display layer with the same settings
    /// (`Shown` takes the world's delay; an explicit other rule is kept), the effects stopped, the notices and the rig
    /// track cleared, and the last view forgotten, so nothing of the old world is drawn, blended or counted as a gap.
    /// The run's counters go on. Returns whether it started over.
    fn enter_world(&mut self, batch: &Batch) -> bool {
        if batch.world == self.world || batch.views.is_empty() {
            return false;
        }
        let mut settings = *self.display.settings();
        if let (DrawnTime::Shown { l_max, .. }, Some(start)) = (settings.drawn_time, batch.start) {
            settings.drawn_time = DrawnTime::Shown { delay: start.display_delay, l_max };
        }
        self.display = DisplayLayer::new(settings);
        self.world_stops.extend(self.effects.new_world());
        if let Some(n) = &mut self.notices {
            n.new_world();
        }
        self.rig.clear();
        self.last = None;
        self.next = None;
        self.drawn = None;
        self.world = batch.world;
        self.world_changed = true;
        true
    }

    /// A batch of a newer world than the one drawn, with no view of it yet: the old world's last view is drawn again.
    fn held(&self, batch: &Batch) -> bool {
        batch.world != self.world && batch.views.is_empty()
    }

    /// The local rig for the display frame about to be drawn over `batch` at `display_time` (render.md 7; crate::rig;
    /// locomotion L4c): the views' rigs resolved at the rule's drawn time, between views or moved on past the newest,
    /// then turned about the head by `local`'s heading lead while the newest view has the local player's own body. Call
    /// it before building the frame's cameras, with the same batch and display time as [`Presenter::frame_batch`],
    /// which then draws the own hands and their joints through it too. `local` is the move feed's newest sample, or
    /// `None` without one. `None` before any view.
    pub fn drawn_rig(
        &mut self,
        batch: &Batch,
        display_time: HostTime,
        local: Option<&MoveInput>,
    ) -> Option<RigTransform> {
        self.enter_world(batch);
        // A gap (views lost between batches, render.md 3) starts the track over (@design-reviewer's L2 on L4c).
        if !batch.views.is_empty() && self.next.is_some_and(|n| n != batch.first) {
            self.rig.clear();
        }
        for v in &batch.views {
            self.rig.push(v);
        }
        if let Some(l) = batch.views.last().or(self.last.as_ref()) {
            self.rig.push(l);
        }
        let rule = self.display.settings().drawn_time;
        let rig = self.rig.resolve(rule.t_draw(display_time), rule.l_max())?;
        let own = batch.views.last().or(self.last.as_ref()).is_some_and(|v| v.heads.iter().any(|h| h.own));
        let rig = match local {
            Some(m) if own => crate::rig::pivot(&rig, m),
            _ => rig,
        };
        self.drawn = Some(rig);
        Some(rig)
    }

    pub fn display(&self) -> &DisplayLayer {
        &self.display
    }

    #[cfg(feature = "audio-trial")]
    pub fn set_audio_observer(&mut self, observer: AudioObserver) {
        self.audio_observer = Some(observer);
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

    /// Show `status`'s line, while it holds one, in every frame from here on.
    pub fn set_status(&mut self, status: Arc<StatusLine>) {
        self.status = Some(status);
    }

    /// Draw the own hands where `switch` says, read at every frame from here on (1.10's `draw latched|simulated`).
    pub fn set_own_hands_switch(&mut self, switch: Arc<OwnHandsSwitch>) {
        self.own_hands = Some(switch);
    }

    /// Once after each switch of where the own hands are drawn: the mode it ended, and the run's counters until then.
    /// The run's counters mix the modes, so a reader takes each mode's window as the difference between these
    /// (@ballpit-viz's L1 on 9057acd0).
    pub fn take_own_hands_window(&mut self) -> Option<(OwnHands, RunReport)> {
        self.own_hands_ended.take()
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

    /// Batches that didn't follow the one before (views lost to the slot's span, or taken by another reader).
    pub fn gaps(&self) -> u64 {
        self.gaps
    }

    /// One display frame over `batch`, the views published since the last frame (`FrameSlot::take`), seen through
    /// `cameras` (both eyes, or the window). Every view in it feeds the display layer, the effect gate and the
    /// notices, in order; the frame draws the newest. An empty batch draws the last view again. `None` before any
    /// view.
    ///
    /// A batch that doesn't start where the last one ended is a gap (render.md 3): the display layer blends every
    /// object from where it was drawn, the effect gate stops what it can no longer know is still on, and the notices
    /// drop what they held pending.
    pub fn frame_batch(
        &mut self,
        batch: &Batch,
        local: &LocalPresentation,
        cameras: &[Camera],
    ) -> Option<PresentedFrame> {
        self.enter_world(batch);
        let held = self.held(batch);
        let gap = !batch.views.is_empty() && self.next.is_some_and(|n| n != batch.first);
        if !batch.views.is_empty() {
            self.next = Some(batch.end());
        }
        let newest = batch.views.last().or(self.last.as_ref())?.clone();
        Some(self.present(&batch.views, gap, held, &newest, local, cameras))
    }

    /// One display frame against `view` alone, seen through `cameras`: the views published between this and the last
    /// call are never read, nor their flags, changes or retractions. For tests and for callers not yet on
    /// [`Presenter::frame_batch`].
    pub fn frame(&mut self, view: &Arc<FrameView>, local: &LocalPresentation, cameras: &[Camera]) -> PresentedFrame {
        let new = self.last.as_ref().is_none_or(|l| !Arc::ptr_eq(l, view));
        let views = if new { std::slice::from_ref(view) } else { &[] };
        self.present(views, false, false, view, local, cameras)
    }

    /// `held`: the frame draws the old world's last view while a new world's first is awaited, so it is counted as
    /// held, not as a display frame (its view only ages).
    fn present(
        &mut self,
        new: &[Arc<FrameView>],
        gap: bool,
        held: bool,
        newest: &Arc<FrameView>,
        local: &LocalPresentation,
        cameras: &[Camera],
    ) -> PresentedFrame {
        let now = local.display_time;
        if let Some(own) = self.own_hands.as_ref().map(|s| s.get())
            && own != self.display.settings().own_hands
        {
            self.own_hands_ended = Some((self.display.settings().own_hands, self.counters.run()));
            self.display.set_own_hands(own);
        }
        let volumes: Vec<_> = cameras.iter().map(Camera::volume).collect();
        let mut effects = std::mem::take(&mut self.world_stops);
        // The audio observer hears a new world as a gap: it drops what it held of the old one.
        #[cfg_attr(not(feature = "audio-trial"), allow(unused_variables))]
        let observer_gap = gap || std::mem::take(&mut self.world_changed);
        if gap {
            self.gaps += 1;
            self.display.gap();
            effects.extend(self.effects.gap(newest));
            if let Some(n) = &mut self.notices {
                n.gap();
            }
        }
        // The batch's earlier views arrive first; `frame` takes the newest.
        for v in new.iter().take(new.len().saturating_sub(1)) {
            self.display.take(v, now, &volumes);
        }
        // The own hands and their joints are drawn through this frame's drawn rig, when the caller resolved one
        // ([`Presenter::drawn_rig`]); else through the newest view's.
        let rig = self.drawn.take();
        self.display.set_drawn_rig(rig);
        let display = self.display.frame(FrameInput { view: newest, display_time: now, volumes: &volumes, local });
        #[cfg(feature = "audio-trial")]
        if let Some(observe) = &mut self.audio_observer {
            observe(&display, newest, new, observer_gap, cameras);
        }
        let taken = self.display.take_counters();
        if held {
            self.counters.held_for_swap(now);
        } else {
            self.counters.display_frame(now, &taken, &display, now);
        }
        // `take` also releases what is due; between arrivals, only the waits run. The gate runs on the drawn time, so
        // under Shown an effect waits for the frame that shows it.
        for v in new {
            if let Some(n) = &mut self.notices {
                n.observe(v, now);
            }
            effects.extend(self.effects.take(v, self.sim_rate_hz, display.t_draw));
        }
        if new.is_empty() {
            effects.extend(self.effects.due(display.t_draw));
        }
        self.last = Some(newest.clone());
        let notices = self.notices.as_mut().map(|n| n.lines(now)).unwrap_or_default();
        let mut scene = Scene::displayed(newest, &display);
        if self.display.settings().own_hands == OwnHands::LateLatched {
            crate::hands::draw_tracked_joints(&mut scene, newest, local, &rig.unwrap_or(newest.local_rig));
        }
        scene.environment = self.environment;
        let status = self.status.as_ref().and_then(|s| s.get());
        PresentedFrame { scene, display, effects, notices, status }
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
            health: pe_seams::Health::Agreed,
            local_rig: RigTransform { pos: Pos::ORIGIN, rotation: [0.0, 0.0, 0.0, 1.0], scale: 1.0 },
            objects: vec![],
            effects: vec![],
            retracted: vec![],
            counters: ViewCounters::default(),
            changes: vec![],
            activations: Vec::new(),
            activations_retracted: Vec::new(),
            hits: Vec::new(),
            hits_from: None,
            heads: Vec::new(),
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

    /// The status line: shown while its holder has one up.
    #[test]
    fn presented_frames_carry_the_status_line_while_it_is_up() {
        let mut p = Presenter::new(DisplaySettings::default(), EffectSettings::default(), Environment::default(), 60);
        let local = LocalPresentation { display_time: HostTime(0), hands: [None, None] };
        assert_eq!(p.frame(&view(1), &local, &[]).status, None, "no status line set");
        let status = Arc::new(StatusLine::default());
        p.set_status(status.clone());
        assert_eq!(p.frame(&view(2), &local, &[]).status, None, "set, but nothing up");
        status.set(Some("Joining: catching up…".into()));
        assert_eq!(p.frame(&view(3), &local, &[]).status.as_deref(), Some("Joining: catching up…"));
        status.set(None);
        assert_eq!(p.frame(&view(4), &local, &[]).status, None, "taken down");
    }

    /// 1.10's `draw latched|simulated`: a switch another thread sets reaches the display layer at the next frame, and
    /// back again; without a switch the setting stays as built.
    #[test]
    fn the_own_hands_switch_reaches_the_display_layer_at_the_next_frame() {
        let mut p = Presenter::new(DisplaySettings::default(), EffectSettings::default(), Environment::default(), 60);
        let local = LocalPresentation { display_time: HostTime(0), hands: [None, None] };
        p.frame(&view(1), &local, &[]);
        assert_eq!(p.display().settings().own_hands, OwnHands::LateLatched, "the default, with no switch");
        let switch = Arc::new(OwnHandsSwitch::new(OwnHands::LateLatched));
        p.set_own_hands_switch(switch.clone());
        switch.set(OwnHands::Simulated);
        assert_eq!(p.display().settings().own_hands, OwnHands::LateLatched, "not before a frame");
        p.frame(&view(2), &local, &[]);
        assert_eq!(p.display().settings().own_hands, OwnHands::Simulated);
        let (ended, run) = p.take_own_hands_window().expect("a window at the switch");
        assert_eq!(ended, OwnHands::LateLatched, "the mode the switch ended");
        assert_eq!(run.display.display_frames, 1, "the run until the switch: frame 1 only");
        assert!(p.take_own_hands_window().is_none(), "taken once");
        switch.set(OwnHands::LateLatched);
        p.frame(&view(3), &local, &[]);
        assert_eq!(p.display().settings().own_hands, OwnHands::LateLatched, "and back");
        assert_eq!(p.take_own_hands_window().map(|w| w.0), Some(OwnHands::Simulated));
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

    /// The slot keeps every view since the last take, in order and numbered, and about 250 ms of them at most.
    #[test]
    fn the_slot_hands_over_every_view_since_the_last_take() {
        let s = FrameSlot::new();
        assert!(s.take().views.is_empty());
        for f in 1..=3 {
            s.publish(view(f));
        }
        let b = s.take();
        assert_eq!((b.first, b.views.iter().map(|v| v.frame.0).collect::<Vec<_>>()), (0, vec![1, 2, 3]));
        assert!(s.take().views.is_empty() && s.take().first == 3, "taken once");
        // A re-publication of one frame is a view of its own.
        s.publish(view(3));
        s.publish(view(4));
        assert_eq!(s.take().views.iter().map(|v| v.frame.0).collect::<Vec<_>>(), [3, 4]);
        // A reader away for 1 s (62 views at 16 ms): only the last 250 ms stay, and the numbers show the hole.
        for f in 5..=66 {
            s.publish(view(f));
        }
        let b = s.take();
        assert_eq!(b.end(), 67);
        assert!(b.first > 5, "the oldest went");
        let span = b.views.last().unwrap().sim_time.0 - b.views[0].sim_time.0;
        assert!(span <= SLOT_SPAN.as_nanos() as i64, "{span} ns kept");
        assert_eq!(b.views.len(), 16, "250 ms at 16 ms a view, both ends included");
    }

    /// Notices and the gap: every view of a batch feeds the notices, so a refusal in a view the newest replaced is
    /// told; a batch after a hole is a gap, and what was pending goes untold.
    #[test]
    fn a_batch_feeds_every_view_and_a_hole_is_a_gap() {
        use crate::notices::NoticeSettings;
        use pe_seams::{Class, EntryRef, Hash, MemberKey, Outcome, Reason, StepRecord};
        let me = MemberKey([1; 32]);
        let refused = |e: u8, f: u64| StepRecord {
            entry: EntryRef(Hash([e; 32])),
            author: me,
            class: Class::Input,
            frame: Frame(f),
            outcome: Outcome::Refused { op: None, reason: Reason::OutOfReach },
        };
        let with = |f: u64, fin: Option<u64>, changes: Vec<StepRecord>| {
            let mut v = (*view(f)).clone();
            v.final_frame = fin.map(Frame);
            v.changes = changes;
            Arc::new(v)
        };
        let mut p = Presenter::new(DisplaySettings::default(), EffectSettings::default(), Environment::default(), 60);
        p.set_notices(Notices::new(NoticeSettings::default(), me, 60));
        let local = LocalPresentation { display_time: HostTime::from_millis(100), hands: [None, None] };
        let slot = FrameSlot::new();
        assert!(p.frame_batch(&slot.take(), &local, &[]).is_none(), "nothing yet");
        // The refusal rides on view 2, which view 3 replaces before the frame; it is final at view 3.
        slot.publish(with(1, None, vec![]));
        slot.publish(with(2, None, vec![refused(7, 2)]));
        slot.publish(with(3, Some(3), vec![]));
        let f = p.frame_batch(&slot.take(), &local, &[]).expect("a frame");
        assert_eq!(f.notices.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), ["Out of reach."]);
        assert_eq!(p.gaps(), 0);
        // An empty batch draws the last view again.
        assert!(p.frame_batch(&slot.take(), &local, &[]).is_some());
        // Refused at view 4, not yet final; then the reader stalls past the slot's span: a gap. The pending refusal
        // goes untold, since a newer record of it may have been among the lost views.
        slot.publish(with(4, Some(3), vec![refused(8, 4)]));
        p.frame_batch(&slot.take(), &local, &[]);
        for f in 5..=40 {
            slot.publish(with(f, Some(f), vec![]));
        }
        let f = p.frame_batch(&slot.take(), &local, &[]).expect("a frame");
        assert_eq!(p.gaps(), 1);
        assert_eq!(p.notices_mut().unwrap().dropped_at_gaps(), 1);
        assert_eq!(f.notices.len(), 1, "only the first refusal's line: {:?}", f.notices);
        assert_eq!(p.notices_mut().unwrap().told_total(), 1);
    }

    /// Another reader took views in between (a handoff between the window and the XR thread): a gap too.
    #[test]
    fn views_taken_by_another_reader_make_a_gap() {
        let mut a = Presenter::new(DisplaySettings::default(), EffectSettings::default(), Environment::default(), 60);
        let mut b = Presenter::new(DisplaySettings::default(), EffectSettings::default(), Environment::default(), 60);
        let local = LocalPresentation { display_time: HostTime::from_millis(100), hands: [None, None] };
        let slot = FrameSlot::new();
        slot.publish(view(1));
        a.frame_batch(&slot.take(), &local, &[]);
        slot.publish(view(2));
        b.frame_batch(&slot.take(), &local, &[]);
        assert_eq!(b.gaps(), 0, "a new reader starts fresh");
        slot.publish(view(3));
        a.frame_batch(&slot.take(), &local, &[]);
        assert_eq!(a.gaps(), 1, "view 2 went to the other reader");
    }
}

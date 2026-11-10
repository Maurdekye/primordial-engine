//! The display layer (render.md 8; divergence.md 8, D1-D8): where each object is drawn between frame views.
//!
//! - **D1, two poses.** The simulated pose comes from S5; resolved to the drawn time (X8) it is the *target*. The
//!   drawn pose is the target composed with a correction offset that decays to zero. The simulation never reads it.
//! - **D2, an offset only from a change of prediction.** When a new frame view arrives, each object's old
//!   prediction and new prediction are both resolved to this display frame's `t_draw` (review M3). Their difference
//!   becomes a new blend component, so the drawn pose never moves at the instant a view arrives, and steady motion
//!   gets no offset at all. Components decay independently, each by a monotone smoothstep, so a further correction
//!   mid-blend never jumps, and the drawn pose reaches the target within one blend time of the last correction.
//! - **D3, by size and by view;** every in-view correction above `E_cue` carries the cue.
//! - **D4, never correct what the player holds:** the own hands and held objects get no offset, ever.
//! - **D5:** remote hands blend like any object.
//! - **D7:** a hold that ends by a lost race blends out of the hand with its cue.
//!
//! One-shot effects (D6) are in [`crate::effects`]. The capture bypasses this layer: it draws simulated poses.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use glam::{DQuat, DVec3};
use pe_seams::{EffectKind, Frame, FrameView, Hand, HostTime, ObjectId, ViewFlags, ViewObject};

use crate::camera::ViewVolume;
use crate::drawn_time::{DrawnTime, extrapolate, resolve};
use crate::pose::{WorldPose, angle_of, offset_vec, rig_to_world};
use crate::{DisplayHand, LocalPresentation};

/// How many frame views the layer keeps for interpolation (X8 (b)). At 60 Hz, 8 views reach 117 ms back.
const HISTORY: usize = 8;
/// A change of prediction below this is float error, not an offset: 1 µm, 1 µrad.
const TOL_POS: f64 = 1e-6;
const TOL_ANG: f64 = 1e-6;
/// At most this many live blend components per object; past it the two oldest merge into one.
const MAX_COMPONENTS: usize = 12;

/// Where the own hands are drawn (D4; xr.md 5.6, X1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OwnHands {
    /// At the late-latched pose (`LocalPresentation::hands`); a held object at that pose times its grip offset.
    #[default]
    LateLatched,
    /// Where simulated (the other setting).
    Simulated,
}

/// The controls of render.md 8 ("controls that bite"): each switches off one protection, so a test can show the
/// counter that guards it rises. Never off in a shipped build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Controls {
    /// Blend corrections. Off: every correction snaps (`snaps_in_view` must rise).
    pub blend: bool,
    /// Attach the cue. Off: `cueless_large_corrections` must rise.
    pub cue: bool,
    /// Treat the own hands and held objects like any object (`own_error` must go non-zero).
    pub correct_own: bool,
}

impl Default for Controls {
    fn default() -> Self {
        Controls { blend: true, cue: true, correct_own: false }
    }
}

/// The display layer's numbers (render.md 8). The [I] values are starting values, tuned at the headset test.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DisplaySettings {
    pub drawn_time: DrawnTime,
    pub own_hands: OwnHands,
    /// `E_small`: 5 cm or 10° [I].
    pub e_small: f64,
    pub a_small: f64,
    /// `E_snap`: 1 m or 90°.
    pub e_snap: f64,
    pub a_snap: f64,
    /// `E_cue`: a hand's width, 10 cm [R meef4f248923b].
    pub e_cue: f64,
    /// `T_blend`, 100 ms; `T_fast`, 40 ms (in view, above `E_snap`).
    pub t_blend: Duration,
    pub t_fast: Duration,
    /// The cue's length (R6): a 250 ms fading outline, plus a ghost at the pose the object left.
    pub t_cue: Duration,
    /// The ghost's opacity at its start; it fades with the outline.
    pub ghost_alpha: f32,
    pub controls: Controls,
}

impl Default for DisplaySettings {
    fn default() -> Self {
        DisplaySettings {
            drawn_time: DrawnTime::default(),
            own_hands: OwnHands::default(),
            e_small: 0.05,
            a_small: 10f64.to_radians(),
            e_snap: 1.0,
            a_snap: 90f64.to_radians(),
            e_cue: 0.10,
            t_blend: Duration::from_millis(100),
            t_fast: Duration::from_millis(40),
            t_cue: Duration::from_millis(250),
            ghost_alpha: 0.35,
            controls: Controls::default(),
        }
    }
}

/// The cue on one drawn object (D3, R6).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cue {
    /// The outline's opacity, 1 at the correction, fading linearly to 0 over `t_cue`.
    pub outline: f32,
    /// A faint copy at the pose the object left, fading with the outline.
    pub ghost: WorldPose,
    pub ghost_alpha: f32,
}

/// One object as drawn this display frame.
#[derive(Clone, Debug, PartialEq)]
pub struct DrawnObject {
    pub id: ObjectId,
    pub pose: WorldPose,
    /// The target pose (the simulated pose at `t_draw`), for counters and tests.
    pub target: WorldPose,
    pub cue: Option<Cue>,
}

/// The display layer's counters (render.md 13; divergence.md 7). Cumulative; the caller reads and resets.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DisplayCounters {
    /// Changes of prediction on objects S5 marked `CORRECTED` (the `corrections_per_min` count).
    pub corrections: u64,
    /// Changes of prediction on objects S5 did not mark: extrapolation misses under X8 (a).
    pub extrapolation_misses: u64,
    /// The largest and the summed extrapolation miss, in metres (`extrapolation_error`).
    pub extrapolation_error_max: f64,
    pub extrapolation_error_sum: f64,
    /// Holds that ended (released, thrown, or lost), each blended out of the hand.
    pub hold_ends: u64,
    /// Corrections shown in view with no blend at all (must stay 0 while blending is on).
    pub snaps_in_view: u64,
    /// Out-of-view corrections above `E_snap`, which jump by design.
    pub jumps_out_of_view: u64,
    pub cues: u64,
    /// In-view corrections above `E_cue` with no cue: must be 0.
    pub cueless_large_corrections: u64,
    /// The largest offset ever applied to an own hand or a held object, in metres (or radians at the bounding
    /// radius): must be exactly 0.
    pub own_error: f64,
    /// Drawn against target, objects in view: the largest, and the sum and count for a mean.
    pub visible_error_max: f64,
    pub visible_error_sum: f64,
    pub visible_samples: u64,
    /// Display frames, and frame views taken.
    pub display_frames: u64,
    pub views_taken: u64,
}

#[derive(Clone, Copy, Debug)]
struct Component {
    d_pos: DVec3,
    d_rot: DQuat,
    start: HostTime,
    dur: Duration,
}

impl Component {
    /// The share of the component still applied at `now`: 1 at its start, 0 after `dur`, a monotone smoothstep.
    fn remaining(&self, now: HostTime) -> f64 {
        let d = self.dur.as_secs_f64();
        if d <= 0.0 {
            return 0.0;
        }
        let u = (now.secs_since(self.start) / d).clamp(0.0, 1.0);
        1.0 - u * u * (3.0 - 2.0 * u)
    }

    fn done(&self, now: HostTime) -> bool {
        now.secs_since(self.start) >= self.dur.as_secs_f64()
    }
}

#[derive(Clone, Copy, Debug)]
struct CueState {
    start: HostTime,
    ghost: WorldPose,
}

/// A correction under X8 (b), waiting until the drawn time reaches the corrected view (see `frame`).
#[derive(Clone, Debug)]
struct Pending {
    frame: Frame,
    at: HostTime,
    /// The object in the view before the corrected one, and that view's time: the old timeline, continued by its
    /// velocity, is the prediction the correction replaces.
    base: ViewObject,
    base_time: HostTime,
}

#[derive(Clone, Debug, Default)]
struct ObjState {
    components: Vec<Component>,
    cue: Option<CueState>,
    /// Held by, or a hand of, the local player in the newest view.
    own: bool,
    last_drawn: Option<WorldPose>,
    pending: Vec<Pending>,
}

impl ObjState {
    fn offset(&self, now: HostTime) -> (DVec3, DQuat) {
        let mut p = DVec3::ZERO;
        let mut q = DQuat::IDENTITY;
        for c in &self.components {
            let r = c.remaining(now);
            p += c.d_pos * r;
            // Oldest outermost, newest innermost (review M1): a new component d = T_old·T_new⁻¹ goes right of
            // the rotation already applied, so Q_old·d·T_new = Q_old·T_old and nothing turns at arrival.
            q *= DQuat::IDENTITY.slerp(c.d_rot, r);
        }
        (p, q.normalize())
    }
}

fn apply(target: WorldPose, (p, q): (DVec3, DQuat)) -> WorldPose {
    target.moved(p).with_rot((q * target.rot).normalize())
}

/// The display layer's state, on the render thread. It survives a lost GPU context (render.md 4).
pub struct DisplayLayer {
    settings: DisplaySettings,
    /// Oldest first.
    history: VecDeque<Arc<FrameView>>,
    objects: BTreeMap<ObjectId, ObjState>,
    /// Under (b): per object, the old timeline's pose at a corrected view, used in place of that view's pose.
    substitutes: BTreeMap<(ObjectId, Frame), WorldPose>,
    counters: DisplayCounters,
}

/// What one display frame resolves against.
pub struct FrameInput<'a> {
    /// The newest frame view in the publication slot. A view not seen before is an arrival.
    pub view: &'a Arc<FrameView>,
    pub display_time: HostTime,
    /// Every current view volume: both eyes, the window.
    pub volumes: &'a [ViewVolume],
    pub local: &'a LocalPresentation,
}

/// One display frame's result.
pub struct DisplayFrame {
    pub t_draw: HostTime,
    /// The extrapolation span used for the newest view, in seconds (render.md 7's counter).
    pub span: f64,
    /// The age of the newest frame view at the display time, in seconds.
    pub view_age: f64,
    /// Sorted by id.
    pub objects: Vec<DrawnObject>,
}

impl DisplayLayer {
    pub fn new(settings: DisplaySettings) -> DisplayLayer {
        DisplayLayer {
            settings,
            history: VecDeque::new(),
            objects: BTreeMap::new(),
            substitutes: BTreeMap::new(),
            counters: DisplayCounters::default(),
        }
    }

    pub fn settings(&self) -> &DisplaySettings {
        &self.settings
    }

    pub fn counters(&self) -> DisplayCounters {
        self.counters
    }

    pub fn take_counters(&mut self) -> DisplayCounters {
        std::mem::take(&mut self.counters)
    }

    /// The newest frame view taken, if any.
    pub fn newest(&self) -> Option<&Arc<FrameView>> {
        self.history.back()
    }

    fn resolve_in(&self, hist: &[&FrameView], id: ObjectId, t_draw: HostTime) -> (Option<WorldPose>, f64) {
        let subs = &self.substitutes;
        let pose_of = |v: &FrameView, o: &ViewObject| subs.get(&(o.id, v.frame)).copied().unwrap_or(WorldPose::of(o));
        let r = resolve(&self.settings.drawn_time, hist, id, t_draw, &pose_of);
        (r.pose, r.span)
    }

    /// One display frame: take the newest view if it is new, then compute every object's drawn pose.
    pub fn frame(&mut self, input: FrameInput<'_>) -> DisplayFrame {
        let now = input.display_time;
        let t_draw = self.settings.drawn_time.t_draw(now);
        self.counters.display_frames += 1;

        let arrived = self.history.back().is_none_or(|v| !Arc::ptr_eq(v, input.view));
        if arrived {
            self.arrive(input.view.clone(), t_draw, now, input.volumes);
        }
        let newest = self.history.back().cloned().expect("a view was taken");
        let owned: Vec<Arc<FrameView>> = self.history.iter().cloned().collect();
        let hist: Vec<&FrameView> = owned.iter().map(|v| &**v).collect();

        // Under (b), corrections wait until the drawn time reaches the corrected view.
        if matches!(self.settings.drawn_time, DrawnTime::InterpolateLate { .. }) {
            self.activate_pending(&hist, &newest, t_draw, now, input.volumes);
        }

        // The own hands' drawn poses first: held objects follow them.
        let mut hand_drawn: BTreeMap<ObjectId, WorldPose> = BTreeMap::new();
        let mut out = Vec::with_capacity(newest.objects.len());
        let span = match self.settings.drawn_time {
            DrawnTime::Extrapolate { l_max } => {
                let cap = l_max.as_secs_f64();
                t_draw.secs_since(newest.sim_time).clamp(-cap, cap)
            }
            DrawnTime::InterpolateLate { .. } => 0.0,
        };
        for o in &newest.objects {
            let (target, _) = self.resolve_in(&hist, o.id, t_draw);
            let Some(target) = target else { continue };
            let st = self.objects.entry(o.id).or_default();
            st.components.retain(|c| !c.done(now));
            if st.cue.is_some_and(|c| now.secs_since(c.start) >= self.settings.t_cue.as_secs_f64()) {
                st.cue = None;
            }
            let own = is_own(o) && !self.settings.controls.correct_own;
            let pose = if is_own(o) && o.hand_of.is_some() {
                let p = own_hand_pose(&self.settings, o, &newest, input.local).unwrap_or(target);
                hand_drawn.insert(o.id, p);
                if own { p } else { apply(p, st.offset(now)) }
            } else if own {
                target
            } else {
                apply(target, st.offset(now))
            };
            if is_own(o) {
                // Measured whatever the control: the offset applied to an own hand or held object (D4).
                let (p, q) = st.offset(now);
                let e = p.length().max(angle_of(q) * o.radius as f64);
                self.counters.own_error = self.counters.own_error.max(e);
            }
            out.push((o, pose, target));
        }
        // Held objects at the hand's drawn pose times the grip offset (D4).
        let mut objects = Vec::with_capacity(out.len());
        for (o, mut pose, target) in out {
            if o.flags.contains(ViewFlags::HELD_LOCAL)
                && !self.settings.controls.correct_own
                && self.settings.own_hands == OwnHands::LateLatched
                && let Some(h) = o.held_by
                && let (Some(hd), Some(hs)) = (hand_drawn.get(&h.hand), newest.object(h.hand))
            {
                let hs = WorldPose::of(hs);
                let grip_pos = hs.rot.inverse() * offset_vec(o.pos - hs.pos);
                let grip_rot = hs.rot.inverse() * WorldPose::of(o).rot;
                pose = hd.moved(hd.rot * grip_pos).with_rot((hd.rot * grip_rot).normalize());
            }
            let st = self.objects.get_mut(&o.id).expect("state made above");
            st.last_drawn = Some(pose);
            let cue = st.cue.map(|c| {
                let f = (1.0 - now.secs_since(c.start) / self.settings.t_cue.as_secs_f64()).clamp(0.0, 1.0) as f32;
                Cue { outline: f, ghost: c.ghost, ghost_alpha: self.settings.ghost_alpha * f }
            });
            if !is_own(o) && input.volumes.iter().any(|v| v.touches_sphere(pose.pos, o.radius as f64)) {
                let e = offset_vec(pose.pos - target.pos).length();
                self.counters.visible_error_max = self.counters.visible_error_max.max(e);
                self.counters.visible_error_sum += e;
                self.counters.visible_samples += 1;
            }
            objects.push(DrawnObject { id: o.id, pose, target, cue });
        }
        DisplayFrame { t_draw, span, view_age: now.secs_since(newest.sim_time), objects }
    }

    /// A new frame view: D2's change of prediction for every object, at this display frame's `t_draw`.
    fn arrive(&mut self, view: Arc<FrameView>, t_draw: HostTime, now: HostTime, volumes: &[ViewVolume]) {
        self.counters.views_taken += 1;
        let old: Vec<Arc<FrameView>> = self.history.iter().cloned().collect();
        self.history.push_back(view.clone());
        while self.history.len() > HISTORY {
            self.history.pop_front();
        }
        let oldest = self.history.front().map(|v| v.frame).unwrap_or_default();
        self.substitutes.retain(|(_, f), _| *f >= oldest);
        let Some(prev) = old.last().cloned() else {
            for o in &view.objects {
                self.objects.entry(o.id).or_default().own = is_own(o);
            }
            return;
        };
        let old_hist: Vec<&FrameView> = old.iter().map(|v| &**v).collect();
        let new_owned: Vec<Arc<FrameView>> = self.history.iter().cloned().collect();
        let new_hist: Vec<&FrameView> = new_owned.iter().map(|v| &**v).collect();
        let lost: Vec<ObjectId> =
            view.effects.iter().filter(|e| e.key.kind == EffectKind::GrabLost).map(|e| e.key.object).collect();
        let interpolating = matches!(self.settings.drawn_time, DrawnTime::InterpolateLate { .. });
        for o in &view.objects {
            let own_now = is_own(o) && !self.settings.controls.correct_own;
            let corrected = o.flags.contains(ViewFlags::CORRECTED);
            let was = self.objects.get(&o.id).cloned();
            let st = self.objects.entry(o.id).or_default();
            let was_own = was.as_ref().is_some_and(|w| w.own);
            st.own = own_now;
            if own_now {
                // D4: a hold starting snaps to the hand; own objects never carry an offset.
                st.components.clear();
                st.cue = None;
                st.pending.clear();
                continue;
            }
            if prev.object(o.id).is_none() {
                continue; // new to the view: nothing to correct from
            }
            let (target_now, _) = self.resolve_in(&new_hist, o.id, t_draw);
            let Some(target_now) = target_now else { continue };
            let (drawn_before, old_target) = if was_own {
                // D4: a hold ended; the object leaves the hand from its last drawn pose.
                self.counters.hold_ends += 1;
                let last = was.and_then(|w| w.last_drawn).unwrap_or(target_now);
                (last, last)
            } else if interpolating {
                // Under (b), only a correction moves the prediction, and it shows only once the drawn time
                // reaches this view. Its pose there is replaced by the old timeline's continuation; the
                // difference blends from that moment (activate_pending).
                if corrected {
                    let base = prev.object(o.id).expect("checked").clone();
                    let cont = extrapolate(&base, view.sim_time.secs_since(prev.sim_time));
                    let st = self.objects.get_mut(&o.id).expect("made above");
                    st.pending.push(Pending { frame: view.frame, at: view.sim_time, base, base_time: prev.sim_time });
                    self.substitutes.insert((o.id, view.frame), cont);
                }
                continue;
            } else {
                let (old_target, _) = {
                    let subs = &self.substitutes;
                    let pose_of =
                        |v: &FrameView, x: &ViewObject| subs.get(&(x.id, v.frame)).copied().unwrap_or(WorldPose::of(x));
                    let r = resolve(&self.settings.drawn_time, &old_hist, o.id, t_draw, &pose_of);
                    (r.pose, r.span)
                };
                let Some(old_target) = old_target else { continue };
                let st = &self.objects[&o.id];
                (apply(old_target, st.offset(now)), old_target)
            };
            let force_cue = was_own && lost.contains(&o.id);
            self.correct(o, old_target, drawn_before, target_now, corrected || was_own, force_cue, now, volumes);
        }
        let ids: Vec<ObjectId> = view.objects.iter().map(|o| o.id).collect();
        self.objects.retain(|id, _| ids.binary_search(id).is_ok());
    }

    /// Under (b): a corrected view's difference starts blending when the drawn time reaches it.
    fn activate_pending(
        &mut self,
        hist: &[&FrameView],
        newest: &FrameView,
        t_draw: HostTime,
        now: HostTime,
        volumes: &[ViewVolume],
    ) {
        let due: Vec<(ObjectId, Pending)> = self
            .objects
            .iter_mut()
            .flat_map(|(id, st)| {
                let (due, keep): (Vec<Pending>, Vec<Pending>) = st.pending.drain(..).partition(|p| p.at <= t_draw);
                st.pending = keep;
                due.into_iter().map(move |p| (*id, p))
            })
            .collect();
        for (id, p) in due {
            let Some(o) = newest.object(id) else {
                self.substitutes.remove(&(id, p.frame));
                continue;
            };
            // Both predictions at this t_draw: the old timeline continued from the view before the correction, and
            // the views as published. Their difference is the offset, so nothing moves now. (The views after the
            // corrected one are on the new timeline too, so the old prediction can't be read from them.)
            self.substitutes.remove(&(id, p.frame));
            let old_target = extrapolate(&p.base, t_draw.secs_since(p.base_time));
            let (Some(new_target), _) = self.resolve_in(hist, id, t_draw) else { continue };
            let drawn_before = apply(old_target, self.objects[&id].offset(now));
            self.correct(o, old_target, drawn_before, new_target, true, false, now, volumes);
        }
    }

    /// Add one change of prediction as a blend component, by D3's rules.
    #[allow(clippy::too_many_arguments)]
    fn correct(
        &mut self,
        o: &ViewObject,
        old_target: WorldPose,
        drawn_before: WorldPose,
        target_now: WorldPose,
        corrected: bool,
        force_cue: bool,
        now: HostTime,
        volumes: &[ViewVolume],
    ) {
        let d_pos = offset_vec(old_target.pos - target_now.pos);
        let d_rot = (old_target.rot * target_now.rot.inverse()).normalize();
        let (e_pos, e_ang) = (d_pos.length(), angle_of(d_rot));
        if e_pos <= TOL_POS && e_ang <= TOL_ANG && !force_cue {
            return;
        }
        let s = self.settings;
        let r = o.radius as f64;
        let e = e_pos.max(e_ang * r);
        if corrected {
            self.counters.corrections += 1;
        } else {
            self.counters.extrapolation_misses += 1;
            self.counters.extrapolation_error_max = self.counters.extrapolation_error_max.max(e_pos);
            self.counters.extrapolation_error_sum += e_pos;
        }
        let in_view =
            volumes.iter().any(|v| v.touches_sphere(drawn_before.pos, r) || v.touches_sphere(target_now.pos, r));
        let big = e_pos > s.e_snap || e_ang > s.a_snap;
        let st = self.objects.get_mut(&o.id).expect("state exists");
        let mut cued = false;
        if in_view && (e > s.e_cue || force_cue) && s.controls.cue {
            st.cue = Some(CueState { start: now, ghost: drawn_before });
            self.counters.cues += 1;
            cued = true;
        }
        if in_view && e > s.e_cue && !cued {
            self.counters.cueless_large_corrections += 1;
        }
        if !in_view && big {
            // Out of every view and very large: jump.
            st.components.clear();
            self.counters.jumps_out_of_view += 1;
            return;
        }
        if !s.controls.blend {
            st.components.clear();
            if in_view {
                self.counters.snaps_in_view += 1;
            }
            return;
        }
        // Below E_small, and between E_small and E_snap out of view, both blend over T_blend (D3); E_small
        // separates the two only for the counters' and the tuning's sake.
        let dur = if in_view && big { s.t_fast } else { s.t_blend };
        st.components.push(Component { d_pos, d_rot, start: now, dur });
        if st.components.len() > MAX_COMPONENTS {
            // Merge the two oldest at their current share, restarting at now: the drawn pose doesn't move.
            let (a, b) = (st.components[0], st.components[1]);
            let (ra, rb) = (a.remaining(now), b.remaining(now));
            let m = Component {
                d_pos: a.d_pos * ra + b.d_pos * rb,
                // The older outer, as in `offset`.
                d_rot: (DQuat::IDENTITY.slerp(a.d_rot, ra) * DQuat::IDENTITY.slerp(b.d_rot, rb)).normalize(),
                start: now,
                dur: a.dur.max(b.dur),
            };
            st.components.splice(0..2, [m]);
        }
    }
}

fn is_own(o: &ViewObject) -> bool {
    o.flags.contains(ViewFlags::OWN_HAND) || o.flags.contains(ViewFlags::HELD_LOCAL)
}

/// The own hand's drawn pose: late-latched from `local` (VR, or the desktop hand's current input pose).
fn own_hand_pose(
    s: &DisplaySettings,
    o: &ViewObject,
    view: &FrameView,
    local: &LocalPresentation,
) -> Option<WorldPose> {
    if s.own_hands != OwnHands::LateLatched || !o.flags.contains(ViewFlags::OWN_HAND) {
        return None;
    }
    let (_, hand) = o.hand_of?;
    let h: &DisplayHand = local.hands[match hand {
        Hand::Left => 0,
        Hand::Right => 1,
    }]
    .as_ref()?;
    Some(rig_to_world(&view.local_rig, &h.pose))
}

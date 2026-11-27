//! The frame-rate counter (the 11-27 friend kit, @coordinator-opus): the frames drawn in the last
//! second, as "N fps", under the wrist button in VR and at the top of the window. It wraps the menu's [`Ui`] (or stands
//! alone without a menu), so it counts the frames the UI is asked for: every drawn frame, in the window and the
//! headset. The label's image changes at most once a second; pe-render writes it into the same texture.

use glam::DVec3;
use pe_render::menu::{MenuPanel, PanelSpec, Ui, UiInput, UiOutput, wrist_place};
use pe_render::pose::WorldPose;
use pe_seams::HostTime;

/// How often the count is renewed.
const PERIOD_S: f64 = 1.0;
/// The label's size against a main panel's, in the headset (about 12 cm wide, the wrist button's width).
const WRIST_SCALE: f64 = 0.35;
/// Below the wrist button, in its plane: half the button, half the label and a gap, in metres at a rig scale of 1.
const BELOW_WRIST: f64 = 0.028;
/// In the window: ahead of the camera, at the top of the view (a vertical field of 1 rad shows 0.27 m above the
/// centre at 0.5 m), and its size against a main panel's.
const WINDOW_AHEAD: f64 = 0.5;
const WINDOW_UP: f64 = 0.22;
const WINDOW_SCALE: f64 = 0.4;

/// Frames counted over each second.
#[derive(Clone, Debug, Default)]
pub struct FpsCounter {
    since: Option<HostTime>,
    frames: u32,
    shown: Option<u32>,
}

impl FpsCounter {
    /// A frame drawn at `now`: the count to show, renewed each second (`None` in the first second).
    pub fn frame(&mut self, now: HostTime) -> Option<u32> {
        let since = *self.since.get_or_insert(now);
        self.frames += 1;
        let span = now.secs_since(since);
        if span >= PERIOD_S {
            // The frames after `since` up to and including this one, over the time between.
            self.shown = Some(((self.frames - 1) as f64 / span).round() as u32);
            self.since = Some(now);
            self.frames = 1;
        }
        self.shown
    }
}

/// The menu with the counter's label.
pub struct FpsUi {
    inner: Option<Box<dyn Ui>>,
    counter: FpsCounter,
    label: Option<MenuPanel>,
}

impl FpsUi {
    pub fn new(inner: Option<Box<dyn Ui>>) -> FpsUi {
        FpsUi { inner, counter: FpsCounter::default(), label: None }
    }

    /// Where the label goes this frame: under the wrist button while the left wrist is tracked, at the top of the
    /// window, or nowhere (the headset with the wrist untracked).
    fn place(input: &UiInput) -> Option<(WorldPose, f64)> {
        if let Some(w) = input.wrist {
            let button = wrist_place(&w, input.scale);
            let pos = button.moved(button.rot * DVec3::new(0.0, -BELOW_WRIST * input.scale, 0.0)).pos;
            return Some((WorldPose { pos, rot: button.rot }, WRIST_SCALE * input.scale));
        }
        // The window draws no rays; the headset always does.
        if input.draw_rays {
            return None;
        }
        let head = input.head;
        let pos = head.moved(head.rot * DVec3::new(0.0, WINDOW_UP, -WINDOW_AHEAD) * input.scale).pos;
        Some((WorldPose { pos, rot: head.rot }, WINDOW_SCALE * input.scale))
    }
}

impl Ui for FpsUi {
    fn frame(&mut self, input: &UiInput) -> UiOutput {
        let mut out = self.inner.as_mut().map(|u| u.frame(input)).unwrap_or_default();
        let fps = self.counter.frame(input.now);
        match (fps, Self::place(input)) {
            (Some(n), Some((pose, scale))) => {
                let spec = PanelSpec { title: None, buttons: Vec::new(), lines: vec![format!("{n} fps")] };
                let p = self.label.get_or_insert_with(|| MenuPanel::new(spec.clone(), pose, scale));
                (p.spec, p.pose, p.scale) = (spec, pose, scale);
                out.overlays.push(p.overlay(None, None));
            }
            _ => self.label = None,
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::DQuat;
    use pe_seams::{Offset, Pos};

    fn ms(t: i64) -> HostTime {
        HostTime::from_millis(t)
    }

    #[test]
    fn the_count_is_the_frames_of_each_second() {
        let mut c = FpsCounter::default();
        // 90 Hz: nothing in the first second, then 90.
        let mut last = None;
        for i in 0..=180 {
            last = c.frame(HostTime(i * 1_000_000_000 / 90));
            if i < 90 {
                assert_eq!(last, None, "frame {i}");
            }
        }
        assert_eq!(last, Some(90));
        // 13 fps from then on: the next second's count says so.
        let start = 2_000;
        for i in 1..=14 {
            last = c.frame(ms(start + i * 1000 / 13));
        }
        assert_eq!(last, Some(13));
    }

    fn input(wrist: Option<WorldPose>, draw_rays: bool, now: HostTime) -> UiInput {
        UiInput {
            now,
            head: WorldPose { pos: Pos::ORIGIN.checked_add(Offset([0.0, 1.6, 0.0])).unwrap(), rot: DQuat::IDENTITY },
            scale: 1.0,
            rays: Vec::new(),
            toggled: false,
            wrist,
            draw_rays,
        }
    }

    #[test]
    fn the_label_shows_in_the_window_and_on_the_wrist_and_keeps_its_image() {
        let mut ui = FpsUi::new(None);
        // The window, 60 Hz: a label after the first second, ahead of the camera and above the centre.
        let mut out = UiOutput::default();
        for i in 0..=61 {
            out = ui.frame(&input(None, false, HostTime(i * 1_000_000_000 / 60)));
        }
        assert_eq!(out.overlays.len(), 1);
        let o = &out.overlays[0];
        let d = o.pose.pos - Pos::ORIGIN;
        assert!((d.0[2] + WINDOW_AHEAD).abs() < 1e-9 && d.0[1] > 1.6, "{d:?}");
        // The same count next frame: the same image, so pe-render uploads nothing.
        let again = ui.frame(&input(None, false, HostTime(62 * 1_000_000_000 / 60)));
        assert!(std::sync::Arc::ptr_eq(&o.image, &again.overlays[0].image));
        // The headset with the wrist untracked: no label. Tracked: one, by the wrist.
        let at = HostTime(63 * 1_000_000_000 / 60);
        assert!(ui.frame(&input(None, true, at)).overlays.is_empty());
        let grip = WorldPose { pos: Pos::ORIGIN.checked_add(Offset([-0.2, 1.0, -0.3])).unwrap(), rot: DQuat::IDENTITY };
        let out = ui.frame(&input(Some(grip), true, HostTime(64 * 1_000_000_000 / 60)));
        assert_eq!(out.overlays.len(), 1);
        let gap = (out.overlays[0].pose.pos - wrist_place(&grip, 1.0).pos).0;
        let len = (gap[0] * gap[0] + gap[1] * gap[1] + gap[2] * gap[2]).sqrt();
        assert!((len - BELOW_WRIST).abs() < 1e-9, "just below the button: {gap:?}");
    }
}

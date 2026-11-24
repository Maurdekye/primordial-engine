//! Menus (render.md 8.2): flat panels of buttons in the world, aimed at with a pointer's ray. Pure CPU: a panel's
//! image, where its buttons are, which button a ray hits, and the ray and cursor that show the aim. The caller (pe-app)
//! owns what the buttons do, where the panels sit, and puts them in `Scene::overlays`.
//!
//! - **The look:** plain for the friend test (the coordinator's working default): a dark translucent panel, one
//!   full-width button per row, centred labels, a title above and text lines below. D-menus' styling comes later.
//! - **A panel's image** is rebuilt only when its content, the hovered button or the pressed button changes.
//! - **Hit-testing** is in the panel's own plane: its quad lies in the pose's XY plane, facing +Z (`Overlay`). A ray
//!   hits only from the front.

use std::sync::Arc;

use ab_glyph::{Font, PxScale, ScaleFont, point};
use glam::{DQuat, DVec3};
use pe_seams::Pos;

use crate::labels::{font, text_width};
use crate::pose::{WorldPose, offset_vec};
use crate::scene::{Overlay, OverlayImage};

/// A panel's width in pixels; its height follows from its rows.
pub const PANEL_PX: u32 = 480;
/// Metres per pixel at a rig scale of 1: a panel is 0.36 m wide.
pub const METRES_PER_PX: f64 = 0.00075;
const TEXT_PX: f32 = 30.0;
const ROW_PX: u32 = 64;
const LINE_PX: u32 = 40;
const MARGIN_PX: u32 = 16;
const GAP_PX: u32 = 10;

/// One button: its caller's id, its label, and whether it can be pressed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Button {
    pub id: u32,
    pub label: String,
    pub enabled: bool,
}

impl Button {
    pub fn new(id: u32, label: impl Into<String>) -> Button {
        Button { id, label: label.into(), enabled: true }
    }
}

/// What a panel shows: an optional title, its buttons, then lines of text (a status, a code).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PanelSpec {
    pub title: Option<String>,
    pub buttons: Vec<Button>,
    pub lines: Vec<String>,
}

/// A panel's layout in pixels: each button's rectangle (left, top, right, bottom), and the image's size.
#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    pub size: [u32; 2],
    pub title: Option<[u32; 4]>,
    pub buttons: Vec<[u32; 4]>,
    pub lines: Vec<[u32; 4]>,
}

/// Where everything goes, top to bottom.
pub fn layout(spec: &PanelSpec) -> Layout {
    let (w, mut y) = (PANEL_PX, MARGIN_PX);
    let row = |y: &mut u32, h: u32| {
        let r = [MARGIN_PX, *y, w - MARGIN_PX, *y + h];
        *y += h + GAP_PX;
        r
    };
    let title = spec.title.as_ref().map(|_| row(&mut y, LINE_PX));
    let buttons = spec.buttons.iter().map(|_| row(&mut y, ROW_PX)).collect();
    let lines = spec.lines.iter().map(|_| row(&mut y, LINE_PX)).collect();
    let h = (y - GAP_PX + MARGIN_PX).max(2 * MARGIN_PX);
    Layout { size: [w, h], title, buttons, lines }
}

/// The colours, linear RGB with an opacity.
const PANEL: ([f32; 3], f32) = ([0.02, 0.02, 0.03], 0.82);
const BUTTON: [f32; 3] = [0.10, 0.11, 0.13];
const HOVER: [f32; 3] = [0.22, 0.25, 0.30];
const PRESSED: [f32; 3] = [0.10, 0.30, 0.55];
const TEXT: [f32; 3] = [1.0, 1.0, 1.0];
const DIM: [f32; 3] = [0.35, 0.35, 0.38];

/// The panel's image with `hover` and `pressed` (button ids) shown.
pub fn render(spec: &PanelSpec, hover: Option<u32>, pressed: Option<u32>) -> OverlayImage {
    let l = layout(spec);
    let mut c = Canvas::new(l.size);
    c.fill([0, 0, l.size[0], l.size[1]], PANEL.0, PANEL.1);
    if let (Some(t), Some(r)) = (&spec.title, l.title) {
        c.text_centred(r, t, TEXT_PX, TEXT);
    }
    for (b, r) in spec.buttons.iter().zip(&l.buttons) {
        let fill = if !b.enabled {
            BUTTON
        } else if pressed == Some(b.id) {
            PRESSED
        } else if hover == Some(b.id) {
            HOVER
        } else {
            BUTTON
        };
        c.fill(*r, fill, 1.0);
        c.text_centred(*r, &b.label, TEXT_PX, if b.enabled { TEXT } else { DIM });
    }
    for (t, r) in spec.lines.iter().zip(&l.lines) {
        c.text_centred(*r, t, TEXT_PX * 0.8, TEXT);
    }
    c.image()
}

/// The button at a point of the panel (`uv`: 0..1 across from the left, 0..1 down from the top), if any. A disabled
/// button is never hit.
pub fn button_at(spec: &PanelSpec, uv: [f64; 2]) -> Option<u32> {
    let l = layout(spec);
    let (x, y) = (uv[0] * l.size[0] as f64, uv[1] * l.size[1] as f64);
    spec.buttons.iter().zip(&l.buttons).find_map(|(b, r)| {
        let inside = x >= r[0] as f64 && x < r[2] as f64 && y >= r[1] as f64 && y < r[3] as f64;
        (inside && b.enabled).then_some(b.id)
    })
}

/// A panel image as made: its content, hover and press, and the image.
type Cached = (PanelSpec, Option<u32>, Option<u32>, Arc<OverlayImage>);

/// A panel in the world: its content, where it sits, and its image, rebuilt only on a change.
#[derive(Clone, Debug)]
pub struct MenuPanel {
    pub spec: PanelSpec,
    /// The panel's centre and orientation (its face toward +Z).
    pub pose: WorldPose,
    /// The rig's scale: the panel is `METRES_PER_PX` per pixel times this.
    pub scale: f64,
    cache: Option<Cached>,
}

impl MenuPanel {
    pub fn new(spec: PanelSpec, pose: WorldPose, scale: f64) -> MenuPanel {
        MenuPanel { spec, pose, scale, cache: None }
    }

    /// Its size in metres.
    pub fn size(&self) -> [f64; 2] {
        let l = layout(&self.spec);
        [l.size[0] as f64 * METRES_PER_PX * self.scale, l.size[1] as f64 * METRES_PER_PX * self.scale]
    }

    /// The overlay this frame, with `hover` and `pressed` shown.
    pub fn overlay(&mut self, hover: Option<u32>, pressed: Option<u32>) -> Overlay {
        let fresh = self.cache.as_ref().is_some_and(|(s, h, p, _)| *s == self.spec && *h == hover && *p == pressed);
        if !fresh {
            let img = Arc::new(render(&self.spec, hover, pressed));
            self.cache = Some((self.spec.clone(), hover, pressed, img));
        }
        let image = self.cache.as_ref().map(|c| c.3.clone()).expect("just made");
        let s = self.size();
        Overlay { image, pose: self.pose, size: [s[0] as f32, s[1] as f32] }
    }

    /// Where a ray from `origin` along `dir` meets the panel's front: its distance and the point's uv.
    pub fn hit(&self, origin: Pos, dir: DVec3) -> Option<RayHit> {
        hit_quad(&self.pose, self.size(), origin, dir)
    }

    /// The enabled button a ray points at, with the hit.
    pub fn aimed(&self, origin: Pos, dir: DVec3) -> Option<(u32, RayHit)> {
        let h = self.hit(origin, dir)?;
        button_at(&self.spec, h.uv).map(|b| (b, h))
    }
}

/// Where a ray met a quad.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RayHit {
    /// Along the ray, in the ray direction's units (metres for a unit direction).
    pub distance: f64,
    /// 0..1 across from the left, 0..1 down from the top.
    pub uv: [f64; 2],
    pub point: Pos,
}

/// A ray against a quad of `size` lying in `pose`'s XY plane, facing +Z: hit only from the front, within the quad.
pub fn hit_quad(pose: &WorldPose, size: [f64; 2], origin: Pos, dir: DVec3) -> Option<RayHit> {
    let inv = pose.rot.normalize().inverse();
    let o = inv * offset_vec(origin - pose.pos);
    let d = inv * dir;
    if o.z <= 0.0 || d.z >= -1e-9 {
        return None;
    }
    let t = -o.z / d.z;
    let p = o + d * t;
    let uv = [p.x / size[0] + 0.5, 0.5 - p.y / size[1]];
    if !(0.0..=1.0).contains(&uv[0]) || !(0.0..=1.0).contains(&uv[1]) {
        return None;
    }
    Some(RayHit { distance: t, uv, point: pose.moved(pose.rot * DVec3::new(p.x, p.y, 0.0)).pos })
}

/// The ray drawn from a hand: a thin quad from `from` to `to`, turned to face `eye` as far as its direction allows.
/// `None` if it is too short to see.
pub fn ray_overlay(from: Pos, to: Pos, eye: Pos, width: f64) -> Option<Overlay> {
    let along = offset_vec(to - from);
    let len = along.length();
    if len < 1e-3 {
        return None;
    }
    let x = along / len;
    let mid = WorldPose { pos: from, rot: DQuat::IDENTITY }.moved(along * 0.5).pos;
    let to_eye = offset_vec(eye - mid);
    let z = (to_eye - x * to_eye.dot(x)).try_normalize().unwrap_or_else(|| x.any_orthonormal_vector());
    let y = z.cross(x);
    let rot = DQuat::from_mat3(&glam::DMat3::from_cols(x, y, z));
    Some(Overlay { image: solid_image(), pose: WorldPose { pos: mid, rot }, size: [len as f32, width as f32] })
}

/// The cursor: a small disc at `at`, facing `eye`.
pub fn cursor_overlay(at: Pos, eye: Pos, diameter: f64) -> Overlay {
    let z = offset_vec(eye - at).try_normalize().unwrap_or(DVec3::Z);
    let rot = DQuat::from_rotation_arc(DVec3::Z, z);
    Overlay { image: disc_image(), pose: WorldPose { pos: at, rot }, size: [diameter as f32; 2] }
}

fn solid_image() -> Arc<OverlayImage> {
    static IMG: std::sync::OnceLock<Arc<OverlayImage>> = std::sync::OnceLock::new();
    IMG.get_or_init(|| Arc::new(OverlayImage { width: 2, height: 2, rgba: [235, 240, 255, 200].repeat(4) })).clone()
}

fn disc_image() -> Arc<OverlayImage> {
    static IMG: std::sync::OnceLock<Arc<OverlayImage>> = std::sync::OnceLock::new();
    IMG.get_or_init(|| {
        let n = 32u32;
        let mut rgba = Vec::with_capacity((n * n * 4) as usize);
        for y in 0..n {
            for x in 0..n {
                let (dx, dy) = (x as f32 + 0.5 - n as f32 / 2.0, y as f32 + 0.5 - n as f32 / 2.0);
                let r = (dx * dx + dy * dy).sqrt() / (n as f32 / 2.0);
                let a = ((1.0 - r) * 8.0).clamp(0.0, 1.0);
                rgba.extend_from_slice(&[255, 255, 255, (a * 255.0).round() as u8]);
            }
        }
        Arc::new(OverlayImage { width: n, height: n, rgba })
    })
    .clone()
}

/// `text` as it fits in `max` pixels at `px`: whole, or cut with an ellipsis.
fn fit(text: &str, px: f32, max: f32) -> String {
    if text_width(text, px) <= max {
        return text.to_string();
    }
    let mut chars: Vec<char> = text.chars().collect();
    loop {
        let t = format!("{}…", chars.iter().collect::<String>());
        if chars.is_empty() || text_width(&t, px) <= max {
            return t;
        }
        chars.pop();
    }
}

// ---- the menus' input and output: what a caller's UI gets each frame ------------------------------------------------

/// One pointer's ray this frame, in the world: a hand's aim (VR) or the mouse's ray (the window).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UiRay {
    /// Which pointer: 0 the left hand, 1 the right hand, 2 the mouse.
    pub pointer: usize,
    pub origin: Pos,
    /// Unit length.
    pub dir: DVec3,
    /// The select is held.
    pub pressed: bool,
    /// Presses since the last frame the UI saw.
    pub clicks: u32,
}

impl UiRay {
    /// A hand's pointer from its aim pose in the rig frame (xr.md 5.7: the ray points along the pose's -Z).
    pub fn from_rig(
        pointer: usize,
        rig: &pe_seams::RigTransform,
        aim: &pe_seams::RigPose,
        pressed: bool,
        clicks: u32,
    ) -> UiRay {
        let w = crate::pose::rig_to_world(rig, aim);
        UiRay { pointer, origin: w.pos, dir: (w.rot * DVec3::NEG_Z).normalize(), pressed, clicks }
    }

    /// The point `len` along the ray.
    pub fn at(&self, len: f64) -> Pos {
        WorldPose { pos: self.origin, rot: DQuat::IDENTITY }.moved(self.dir * len).pos
    }
}

/// A main panel's pose: `ahead` in front of the head along its level forward and `down` below it (both times the
/// rig's scale), upright, facing the head.
pub fn place_in_front(head: &WorldPose, ahead: f64, down: f64, scale: f64) -> WorldPose {
    let f = head.rot * DVec3::NEG_Z;
    let level = DVec3::new(f.x, 0.0, f.z).try_normalize().unwrap_or(DVec3::NEG_Z);
    let rot = DQuat::from_rotation_arc(DVec3::NEG_Z, level);
    WorldPose { pos: head.moved(level * ahead * scale + DVec3::NEG_Y * down * scale).pos, rot }
}

/// A side panel's pose: beside `main`, `across` to its right (negative: its left), turned `turn_deg` toward the head.
pub fn place_beside(main: &WorldPose, across: f64, turn_deg: f64) -> WorldPose {
    let rot = main.rot * DQuat::from_rotation_y((-across.signum() * turn_deg).to_radians());
    WorldPose { pos: main.moved(main.rot * DVec3::new(across, 0.0, 0.0)).pos, rot }
}

/// The wrist button's pose from the left hand's grip pose (xr.md 5.7): on the back of the wrist,
/// the grip's +Y side and toward the wrist along its +Z, its face out along the grip's +Y.
pub fn wrist_place(grip: &WorldPose, scale: f64) -> WorldPose {
    let rot = grip.rot * DQuat::from_rotation_x(-std::f64::consts::FRAC_PI_2);
    WorldPose { pos: grip.moved(grip.rot * DVec3::new(0.0, 0.035, 0.09) * scale).pos, rot }
}

/// What a UI gets each drawn frame.
#[derive(Clone, Debug)]
pub struct UiInput {
    pub now: pe_seams::HostTime,
    /// The head (VR) or the camera (the window), in the world.
    pub head: WorldPose,
    /// The rig's scale: panels are sized by it.
    pub scale: f64,
    pub rays: Vec<UiRay>,
    /// The menu was toggled since the last frame (a controller's menu button, the window's key).
    pub toggled: bool,
    /// The left hand's grip pose in the world (VR), for a wrist button; `None` in the window or while untracked.
    pub wrist: Option<WorldPose>,
    /// Draw each ray from its origin (VR); the window's mouse ray isn't drawn.
    pub draw_rays: bool,
}

/// What a UI gives back: its overlays, drawn after the notices, and which pointers' presses it took (the window then
/// doesn't also grab with them).
#[derive(Clone, Debug, Default)]
pub struct UiOutput {
    pub overlays: Vec<Overlay>,
    pub took: [bool; 3],
}

/// A caller's UI over the menus: pe-app's.
pub trait Ui: Send {
    fn frame(&mut self, input: &UiInput) -> UiOutput;
}

/// An image being drawn: premultiplied linear RGBA.
struct Canvas {
    size: [u32; 2],
    px: Vec<[f32; 4]>,
}

impl Canvas {
    fn new(size: [u32; 2]) -> Canvas {
        Canvas { size, px: vec![[0.0; 4]; (size[0] * size[1]) as usize] }
    }

    fn over(&mut self, x: u32, y: u32, c: [f32; 3], a: f32) {
        let p = &mut self.px[(y * self.size[0] + x) as usize];
        for (k, v) in c.iter().enumerate() {
            p[k] = v * a + p[k] * (1.0 - a);
        }
        p[3] = a + p[3] * (1.0 - a);
    }

    fn fill(&mut self, r: [u32; 4], c: [f32; 3], a: f32) {
        for y in r[1]..r[3].min(self.size[1]) {
            for x in r[0]..r[2].min(self.size[0]) {
                self.over(x, y, c, a);
            }
        }
    }

    /// `text` centred in `r`, cut with an ellipsis if it doesn't fit.
    fn text_centred(&mut self, r: [u32; 4], text: &str, px: f32, c: [f32; 3]) {
        let max = (r[2] - r[0]) as f32 - 2.0 * GAP_PX as f32;
        let t = fit(text, px, max);
        let f = font().as_scaled(PxScale::from(px));
        let left = r[0] as f32 + ((r[2] - r[0]) as f32 - text_width(&t, px)) / 2.0;
        let base = r[1] as f32 + ((r[3] - r[1]) as f32 + f.ascent() + f.descent()) / 2.0;
        let mut pen = point(left, base);
        let mut prev = None;
        for ch in t.chars() {
            let id = f.glyph_id(ch);
            if let Some(p) = prev {
                pen.x += f.kern(p, id);
            }
            let g = id.with_scale_and_position(px, pen);
            pen.x += f.h_advance(id);
            prev = Some(id);
            if let Some(o) = f.outline_glyph(g) {
                let b = o.px_bounds();
                let mut cov = Vec::new();
                o.draw(|gx, gy, v| cov.push((b.min.x as i64 + gx as i64, b.min.y as i64 + gy as i64, v)));
                for (x, y, v) in cov {
                    if x >= 0 && y >= 0 && (x as u32) < self.size[0] && (y as u32) < self.size[1] {
                        self.over(x as u32, y as u32, c, v.clamp(0.0, 1.0));
                    }
                }
            }
        }
    }

    fn image(&self) -> OverlayImage {
        let srgb = |l: f32| {
            let l = l.clamp(0.0, 1.0);
            let s = if l <= 0.003_130_8 { l * 12.92 } else { 1.055 * l.powf(1.0 / 2.4) - 0.055 };
            (s * 255.0).round() as u8
        };
        let mut rgba = Vec::with_capacity(self.px.len() * 4);
        for p in &self.px {
            let a = p[3];
            let un = |v: f32| if a > 0.0 { v / a } else { 0.0 };
            rgba.extend_from_slice(&[srgb(un(p[0])), srgb(un(p[1])), srgb(un(p[2])), (a * 255.0).round() as u8]);
        }
        OverlayImage { width: self.size[0], height: self.size[1], rgba }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pe_seams::Offset;

    fn at(d: [f64; 3]) -> Pos {
        Pos::ORIGIN.checked_add(Offset(d)).unwrap()
    }

    fn spec() -> PanelSpec {
        PanelSpec {
            title: Some("Primordial Engine".into()),
            buttons: vec![
                Button::new(1, "Invite a friend"),
                Button::new(2, "Join"),
                Button { enabled: false, ..Button::new(3, "Quit") },
            ],
            lines: vec!["Your session".into()],
        }
    }

    #[test]
    fn a_long_text_is_cut_with_an_ellipsis_to_fit() {
        assert_eq!(fit("Join", 30.0, 400.0), "Join");
        let long = "pe1:0123456789abcdef/0123456789abcdef0123456789abcdef/1767225600/192.168.1.20:4433";
        let t = fit(long, 30.0, 400.0);
        assert!(t.ends_with('…') && text_width(&t, 30.0) <= 400.0 && t.chars().count() > 5, "{t}");
    }

    #[test]
    fn the_layout_stacks_title_buttons_and_lines() {
        let l = layout(&spec());
        assert_eq!(l.size[0], PANEL_PX);
        assert_eq!(l.buttons.len(), 3);
        let t = l.title.unwrap();
        assert!(t[3] <= l.buttons[0][1] && l.buttons[2][3] <= l.lines[0][1], "top to bottom");
        assert!(l.lines[0][3] + MARGIN_PX <= l.size[1]);
    }

    #[test]
    fn a_point_names_the_button_under_it_and_never_a_disabled_one() {
        let s = spec();
        let l = layout(&s);
        let centre = |r: [u32; 4]| {
            [(r[0] + r[2]) as f64 / 2.0 / l.size[0] as f64, (r[1] + r[3]) as f64 / 2.0 / l.size[1] as f64]
        };
        assert_eq!(button_at(&s, centre(l.buttons[0])), Some(1));
        assert_eq!(button_at(&s, centre(l.buttons[1])), Some(2));
        assert_eq!(button_at(&s, centre(l.buttons[2])), None, "disabled");
        assert_eq!(button_at(&s, centre(l.title.unwrap())), None, "the title is no button");
        assert_eq!(button_at(&s, [0.005, 0.5]), None, "the margin");
    }

    #[test]
    fn a_ray_hits_the_front_of_a_panel_where_it_points() {
        // A panel 1 m ahead (-Z), facing back toward the origin (+Z).
        let p = MenuPanel::new(spec(), WorldPose { pos: at([0.0, 1.5, -1.0]), rot: DQuat::IDENTITY }, 1.0);
        let eye = at([0.0, 1.5, 0.0]);
        let h = p.hit(eye, DVec3::new(0.0, 0.0, -1.0)).expect("straight ahead");
        assert!((h.distance - 1.0).abs() < 1e-9 && (h.uv[0] - 0.5).abs() < 1e-9 && (h.uv[1] - 0.5).abs() < 1e-9);
        // From behind: no hit. Pointing away: no hit. Past the edge: no hit.
        assert!(p.hit(at([0.0, 1.5, -2.0]), DVec3::new(0.0, 0.0, 1.0)).is_none());
        assert!(p.hit(eye, DVec3::new(0.0, 0.0, 1.0)).is_none());
        assert!(p.hit(eye, DVec3::new(1.0, 0.0, -1.0)).is_none());
        // Aim at the first button's centre.
        let l = layout(&p.spec);
        let s = p.size();
        let b = l.buttons[0];
        let y = s[1] * (0.5 - (b[1] + b[3]) as f64 / 2.0 / l.size[1] as f64);
        let (id, _) = p.aimed(eye, DVec3::new(0.0, y, -1.0)).expect("on a button");
        assert_eq!(id, 1);
        // A turned panel: rotated 90 degrees about Y, so its face looks along +X; hit from +X.
        let q = MenuPanel::new(
            spec(),
            WorldPose { pos: at([1.0, 1.5, 0.0]), rot: DQuat::from_rotation_y(std::f64::consts::FRAC_PI_2) },
            1.0,
        );
        assert!(q.hit(at([2.0, 1.5, 0.0]), DVec3::new(-1.0, 0.0, 0.0)).is_some());
        assert!(q.hit(at([0.0, 1.5, 0.0]), DVec3::new(1.0, 0.0, 0.0)).is_none(), "its back");
    }

    #[test]
    fn the_image_shows_hover_and_press_and_is_rebuilt_only_on_a_change() {
        let s = spec();
        let l = layout(&s);
        let px = |img: &OverlayImage, r: [u32; 4]| {
            let (x, y) = (r[0] + 4, r[1] + 4);
            let i = ((y * img.width + x) * 4) as usize;
            img.rgba[i]
        };
        let plain = render(&s, None, None);
        let hover = render(&s, Some(1), None);
        let press = render(&s, Some(1), Some(1));
        assert_eq!((plain.width, plain.height), (l.size[0], l.size[1]));
        assert!(px(&hover, l.buttons[0]) > px(&plain, l.buttons[0]), "hover lightens");
        assert_ne!(press.rgba, hover.rgba, "press shows");
        assert_eq!(px(&hover, l.buttons[1]), px(&plain, l.buttons[1]), "only the hovered button changes");
        let mut p = MenuPanel::new(s, WorldPose { pos: at([0.0, 1.5, -1.0]), rot: DQuat::IDENTITY }, 1.0);
        let a = p.overlay(None, None).image;
        assert!(Arc::ptr_eq(&a, &p.overlay(None, None).image), "unchanged: the same image");
        assert!(!Arc::ptr_eq(&a, &p.overlay(Some(2), None).image), "hover: a new image");
        p.spec.lines = vec!["Joining…".into()];
        assert!(!Arc::ptr_eq(&a, &p.overlay(None, None).image), "new content: a new image");
    }

    #[test]
    fn the_ray_runs_from_the_hand_to_the_hit_and_the_cursor_faces_the_eye() {
        let (from, to, eye) = (at([0.2, 1.2, -0.3]), at([0.0, 1.5, -1.0]), at([0.0, 1.6, 0.0]));
        let r = ray_overlay(from, to, eye, 0.004).expect("long enough");
        assert!((r.size[0] as f64 - offset_vec(to - from).length()).abs() < 1e-6);
        let x = r.pose.rot * DVec3::X;
        assert!((x - offset_vec(to - from).normalize()).length() < 1e-9, "its length runs along the ray");
        assert!(ray_overlay(from, from, eye, 0.004).is_none());
        let c = cursor_overlay(to, eye, 0.02);
        let z = c.pose.rot * DVec3::Z;
        assert!((z - offset_vec(eye - to).normalize()).length() < 1e-9, "the disc faces the eye");
    }
}

//! Names beside objects in a capture: `view_capture`'s `label: true` (render.md 11; api.md 8).
//!
//! - **Who knows the names:** the caller (pe-api reads them from the snapshot). pe-render gets `(id, name)` pairs.
//! - **Where they go:** [`place_labels`] runs when the capture is recorded, with its scene and camera. Each label
//!   sits beside its object's anchor: the projected circle of its bounding sphere, cut to `visible_list`'s box
//!   (that box is built from a cube around the sphere, so it is up to √3 too wide to sit a label against). The
//!   label goes to the right, else to the left, above or below: the first spot inside the image that covers no
//!   other label and no other named object; failing that, the first that covers no label. An object the camera
//!   doesn't see gets no label.
//! - **Drawing:** [`Image::draw_labels`] draws them on the CPU, onto the read-back image, on the same worker thread
//!   as the PNG encoding, so they never cost the render thread. White text on a dark box, blended in linear light.
//! - **The font:** Inter Regular, embedded (SIL OFL 1.1, `fonts/Inter-LICENSE.txt`). It covers Latin, Greek and
//!   Cyrillic; a character it lacks is drawn as its missing-glyph box, never dropped silently. A name too wide for
//!   the image is cut and ends with an ellipsis.

use std::sync::OnceLock;

use ab_glyph::{Font, FontRef, PxScale, ScaleFont, point};
use pe_seams::ObjectId;

use crate::camera::Camera;
use crate::capture::{Image, visible_list};
use crate::scene::Scene;

static FONT_BYTES: &[u8] = include_bytes!("../fonts/Inter-Regular.ttf");

fn font() -> &'static FontRef<'static> {
    static FONT: OnceLock<FontRef<'static>> = OnceLock::new();
    FONT.get_or_init(|| FontRef::try_from_slice(FONT_BYTES).expect("the embedded font parses"))
}

/// Space between an object's box and its label, and inside the label's box around the text, in pixels.
const GAP: f32 = 4.0;
const PAD: f32 = 3.0;
/// The label box's fill (linear RGB, opacity) and the text colour (linear RGB).
const BOX: ([f32; 3], f32) = ([0.0, 0.0, 0.0], 0.62);
const TEXT: [f32; 3] = [1.0, 1.0, 1.0];

/// One label, placed: the text as drawn (cut to fit), its box and the baseline it is drawn on.
#[derive(Clone, Debug, PartialEq)]
pub struct LabelPlace {
    pub id: ObjectId,
    pub text: String,
    /// The label's box in pixels: left, top, right, bottom, inside the image.
    pub rect: [f32; 4],
    /// The text size in pixels.
    pub px: f32,
}

/// The text size for an image of this height: 18 px at the default 432, between 12 and 28.
pub fn text_px(height: u32) -> f32 {
    (height as f32 / 24.0).clamp(12.0, 28.0)
}

/// The width of `text` at `px`, with kerning.
fn text_width(text: &str, px: f32) -> f32 {
    let f = font().as_scaled(PxScale::from(px));
    let mut w = 0.0;
    let mut prev = None;
    for c in text.chars() {
        let g = f.glyph_id(c);
        if let Some(p) = prev {
            w += f.kern(p, g);
        }
        w += f.h_advance(g);
        prev = Some(g);
    }
    w
}

/// `text`, cut and ended with an ellipsis if it is wider than `max` at `px`.
fn fit(text: &str, px: f32, max: f32) -> String {
    if text_width(text, px) <= max {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    for n in (0..chars.len()).rev() {
        let s: String = chars[..n].iter().chain(std::iter::once(&'…')).collect();
        if text_width(&s, px) <= max {
            return s;
        }
    }
    String::new()
}

fn overlaps(a: &[f32; 4], b: &[f32; 4]) -> bool {
    a[0] < b[2] && b[0] < a[2] && a[1] < b[3] && b[1] < a[3]
}

/// Where each named object's label goes in a capture of `scene` from `camera` at `size`. The order of `names` is the
/// order of preference when labels compete for space. Objects out of view, and empty names, get none.
pub fn place_labels(scene: &Scene, camera: &Camera, size: [u32; 2], names: &[(ObjectId, String)]) -> Vec<LabelPlace> {
    let (w, h) = (size[0] as f32, size[1] as f32);
    let px = text_px(size[1]);
    let line = {
        let f = font().as_scaled(PxScale::from(px));
        f.ascent() - f.descent()
    };
    let bh = line + 2.0 * PAD;
    if bh > h {
        return Vec::new();
    }
    let seen = visible_list(scene, camera, size);
    let proj = camera.projection();
    // The projected circle of an object's bounding sphere, cut to its visible_list box.
    let anchor = |id: ObjectId| -> Option<[f32; 4]> {
        let v = seen.iter().find(|v| v.id == id)?;
        let it = scene.items.iter().find(|i| i.id == id)?;
        let c = camera.relative(it.pose.pos);
        if c.z >= -camera.near {
            return Some(v.rect);
        }
        let p = proj * glam::Vec4::new(c.x as f32, c.y as f32, c.z as f32, 1.0);
        let (x, y) = ((p.x / p.w * 0.5 + 0.5) * w, (0.5 - p.y / p.w * 0.5) * h);
        let rp = it.radius * proj.y_axis.y / (-c.z as f32) * h * 0.5;
        let a = [(x - rp).max(v.rect[0]), (y - rp).max(v.rect[1]), (x + rp).min(v.rect[2]), (y + rp).min(v.rect[3])];
        Some(if a[2] > a[0] && a[3] > a[1] { a } else { v.rect })
    };
    let anchors: Vec<(ObjectId, [f32; 4])> = names.iter().filter_map(|(id, _)| Some((*id, anchor(*id)?))).collect();
    let mut out: Vec<LabelPlace> = Vec::new();
    for (id, name) in names {
        let Some(&(_, r)) = anchors.iter().find(|(a, _)| a == id) else { continue };
        let text = fit(name, px, w - 2.0 * PAD);
        if text.is_empty() {
            continue;
        }
        let bw = text_width(&text, px) + 2.0 * PAD;
        let (cx, cy) = ((r[0] + r[2]) / 2.0, (r[1] + r[3]) / 2.0);
        let candidates = [
            [r[2] + GAP, cy - bh / 2.0],
            [r[0] - GAP - bw, cy - bh / 2.0],
            [cx - bw / 2.0, r[1] - GAP - bh],
            [cx - bw / 2.0, r[3] + GAP],
        ];
        let boxed = |[x, y]: [f32; 2]| [x, y, x + bw, y + bh];
        let inside = |b: &[f32; 4]| b[0] >= 0.0 && b[1] >= 0.0 && b[2] <= w && b[3] <= h;
        let clear = |b: &[f32; 4]| out.iter().all(|o| !overlaps(b, &o.rect));
        let uncovering = |b: &[f32; 4]| anchors.iter().all(|(a, ar)| a == id || !overlaps(b, ar));
        let clamped = |[x, y]: [f32; 2]| boxed([x.clamp(0.0, w - bw), y.clamp(0.0, h - bh)]);
        let rect = candidates
            .iter()
            .map(|&c| boxed(c))
            .find(|b| inside(b) && clear(b) && uncovering(b))
            .or_else(|| candidates.iter().map(|&c| boxed(c)).find(|b| inside(b) && clear(b)))
            .or_else(|| candidates.iter().map(|&c| clamped(c)).find(|b| clear(b)))
            .unwrap_or_else(|| clamped(candidates[0]));
        out.push(LabelPlace { id: *id, text, rect: rect.map(f32::round), px });
    }
    out
}

fn to_linear(c: u8) -> f32 {
    let c = c as f32 / 255.0;
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

fn to_srgb(l: f32) -> u8 {
    let l = l.clamp(0.0, 1.0);
    let c = if l <= 0.003_130_8 { l * 12.92 } else { 1.055 * l.powf(1.0 / 2.4) - 0.055 };
    (c * 255.0 + 0.5) as u8
}

impl Image {
    fn blend(&mut self, x: i64, y: i64, color: [f32; 3], a: f32) {
        if x < 0 || y < 0 || x >= self.width as i64 || y >= self.height as i64 || a <= 0.0 {
            return;
        }
        let i = ((y as usize) * self.width as usize + x as usize) * 4;
        for (byte, c) in self.rgba[i..i + 3].iter_mut().zip(color) {
            let d = to_linear(*byte);
            *byte = to_srgb(d + (c - d) * a.min(1.0));
        }
    }

    /// Draw labels placed by [`place_labels`] for this image's size. Pure CPU; run it beside the PNG encoding.
    pub fn draw_labels(&mut self, labels: &[LabelPlace]) {
        for l in labels {
            let [x0, y0, x1, y1] = l.rect.map(|v| v as i64);
            for y in y0..y1 {
                for x in x0..x1 {
                    self.blend(x, y, BOX.0, BOX.1);
                }
            }
            let f = font().as_scaled(PxScale::from(l.px));
            let mut pen = point(l.rect[0] + PAD, l.rect[1] + PAD + f.ascent());
            let mut prev = None;
            for c in l.text.chars() {
                let id = f.glyph_id(c);
                if let Some(p) = prev {
                    pen.x += f.kern(p, id);
                }
                let g = id.with_scale_and_position(l.px, pen);
                pen.x += f.h_advance(id);
                prev = Some(id);
                if let Some(o) = f.outline_glyph(g) {
                    let b = o.px_bounds();
                    o.draw(|gx, gy, cov| {
                        self.blend(b.min.x as i64 + gx as i64, b.min.y as i64 + gy as i64, TEXT, cov);
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera::{DepthRange, Fov};
    use crate::pose::WorldPose;
    use crate::scene::{DEFAULT_APPEARANCE, DrawItem};
    use glam::DQuat;
    use pe_seams::{Offset, Pos, Shape};

    fn ball(id: u128, at: [f64; 3], r: f32) -> DrawItem {
        DrawItem {
            id: ObjectId::from_u128(id),
            pose: WorldPose { pos: Pos::ORIGIN.checked_add(Offset(at)).unwrap(), rot: DQuat::IDENTITY },
            scale: [1.0; 3],
            shape: Shape::Sphere { radius: r },
            appearance: DEFAULT_APPEARANCE,
            radius: r,
            outline: None,
            ghost: None,
        }
    }

    fn camera() -> Camera {
        Camera::placed(
            WorldPose { pos: Pos::ORIGIN, rot: DQuat::IDENTITY },
            Fov::symmetric(1.0, 16.0 / 9.0),
            DepthRange::DESKTOP,
        )
    }

    const SIZE: [u32; 2] = [768, 432];

    fn scene(items: Vec<DrawItem>) -> Scene {
        Scene { items, ..Scene::default() }
    }

    fn names(pairs: &[(u128, &str)]) -> Vec<(ObjectId, String)> {
        pairs.iter().map(|(i, s)| (ObjectId::from_u128(*i), s.to_string())).collect()
    }

    #[test]
    fn a_label_sits_to_the_right_of_its_object() {
        let s = scene(vec![ball(1, [0.0, 0.0, -4.0], 0.3)]);
        let v = visible_list(&s, &camera(), SIZE)[0];
        let p = place_labels(&s, &camera(), SIZE, &names(&[(1, "Lamp")]));
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].text, "Lamp");
        assert!(p[0].rect[0] >= v.rect[2], "{:?} vs {:?}", p[0].rect, v.rect);
        let (cy, ly) = ((v.rect[1] + v.rect[3]) / 2.0, (p[0].rect[1] + p[0].rect[3]) / 2.0);
        assert!((cy - ly).abs() <= 1.0);
        assert_eq!(p[0].px, 18.0);
    }

    #[test]
    fn at_the_right_edge_the_label_goes_left() {
        // Near the right edge of the view: x/z just inside the half-width (tan 0.5 * 16/9 = 0.97).
        let s = scene(vec![ball(1, [3.6, 0.0, -4.0], 0.2)]);
        let v = visible_list(&s, &camera(), SIZE)[0];
        let p = place_labels(&s, &camera(), SIZE, &names(&[(1, "A rather long name")]));
        // Left of the ball's centre, inside the image.
        assert!(p[0].rect[2] <= (v.rect[0] + v.rect[2]) / 2.0, "{:?} vs {:?}", p[0].rect, v.rect);
        assert!(p[0].rect[0] >= 0.0 && p[0].rect[2] <= SIZE[0] as f32);
    }

    #[test]
    fn labels_never_overlap_and_unseen_objects_get_none() {
        let s = scene(vec![
            ball(1, [0.0, 0.0, -4.0], 0.3),
            ball(2, [0.0, 0.05, -4.2], 0.3),
            ball(3, [0.05, -0.05, -3.9], 0.3),
            ball(4, [0.0, 0.0, 4.0], 0.3), // behind the camera
        ]);
        let p = place_labels(&s, &camera(), SIZE, &names(&[(1, "One"), (2, "Two"), (3, "Three"), (4, "Behind")]));
        let ids: Vec<ObjectId> = p.iter().map(|l| l.id).collect();
        assert_eq!(ids, [1, 2, 3].map(ObjectId::from_u128));
        for (i, a) in p.iter().enumerate() {
            for b in &p[i + 1..] {
                assert!(!overlaps(&a.rect, &b.rect), "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn a_label_avoids_covering_another_named_object() {
        // Ball 2 sits just right of ball 1, where ball 1's label would go first.
        let s = scene(vec![ball(1, [0.0, 0.0, -4.0], 0.3), ball(2, [0.75, 0.0, -4.0], 0.3)]);
        let cam = camera();
        let p = place_labels(&s, &cam, SIZE, &names(&[(1, "One"), (2, "Two")]));
        let v2 = visible_list(&s, &cam, SIZE).into_iter().find(|v| v.id == ObjectId::from_u128(2)).unwrap();
        let v1 = visible_list(&s, &cam, SIZE).into_iter().find(|v| v.id == ObjectId::from_u128(1)).unwrap();
        assert!(p[0].rect[2] <= v1.rect[0] || p[0].rect[3] <= v1.rect[1] || p[0].rect[1] >= v1.rect[3], "{p:?}");
        assert!(!overlaps(&p[0].rect, &v2.rect) || p[0].rect[2] <= v2.rect[0], "{p:?} covers {v2:?}");
    }

    #[test]
    fn the_anchor_is_the_sphere_not_its_bounding_cube() {
        // Seen from the side the cube's box is wider than the sphere's circle; the label sits at the circle.
        let s = scene(vec![ball(1, [1.2, 0.0, -3.0], 0.5)]);
        let cam = camera();
        let v = visible_list(&s, &cam, SIZE)[0];
        let p = place_labels(&s, &cam, SIZE, &names(&[(1, "One")]));
        let proj = cam.projection();
        let c = glam::Vec4::new(1.2, 0.0, -3.0, 1.0);
        let q = proj * c;
        let right = ((q.x / q.w) * 0.5 + 0.5) * SIZE[0] as f32 + 0.5 * proj.y_axis.y / 3.0 * SIZE[1] as f32 * 0.5;
        assert!(right < v.rect[2] - 2.0, "the cube's box ({}) should be wider than the circle ({right})", v.rect[2]);
        assert!((p[0].rect[0] - (right + GAP)).abs() <= 1.0, "{:?} against {right}", p[0].rect);
    }

    #[test]
    fn a_name_wider_than_the_image_is_cut_with_an_ellipsis() {
        let s = scene(vec![ball(1, [0.0, 0.0, -4.0], 0.3)]);
        let long = "W".repeat(200);
        let p = place_labels(&s, &camera(), [160, 90], &names(&[(1, &long)]));
        assert!(p[0].text.ends_with('…') && p[0].text.chars().count() < 200, "{}", p[0].text);
        assert!(p[0].rect[0] >= 0.0 && p[0].rect[2] <= 160.0);
    }

    #[test]
    fn drawing_touches_only_the_label_box_and_writes_light_text() {
        let s = scene(vec![ball(1, [0.0, 0.0, -4.0], 0.3)]);
        let p = place_labels(&s, &camera(), SIZE, &names(&[(1, "Lamp")]));
        let grey = 128u8;
        let mut img = Image { width: SIZE[0], height: SIZE[1], rgba: vec![grey; (SIZE[0] * SIZE[1] * 4) as usize] };
        img.draw_labels(&p);
        let r = p[0].rect;
        let (mut inside_max, mut changed_outside) = (0u8, 0);
        for y in 0..SIZE[1] {
            for x in 0..SIZE[0] {
                let v = img.rgba[((y * SIZE[0] + x) * 4) as usize];
                let inside = (x as f32) >= r[0] && (x as f32) < r[2] && (y as f32) >= r[1] && (y as f32) < r[3];
                if inside {
                    inside_max = inside_max.max(v);
                } else if v != grey {
                    changed_outside += 1;
                }
            }
        }
        assert_eq!(changed_outside, 0);
        assert!(inside_max > 240, "text is near white, got {inside_max}");
        // The box darkens the background where there is no text: its top-left pixel is inside the padding.
        let corner = img.rgba[((r[1] as u32 * SIZE[0] + r[0] as u32) * 4) as usize];
        assert!(corner < grey, "{corner}");
        // Alpha stays untouched.
        assert!(img.rgba.chunks(4).all(|p| p[3] == grey));
    }

    #[test]
    fn a_missing_glyph_still_draws_something() {
        // U+4E00 is not in Inter: it falls back to the missing-glyph box, never to nothing.
        assert!(text_width("\u{4e00}", 18.0) > 0.0);
        assert!(text_width("Lamp", 18.0) > text_width("La", 18.0));
    }
}

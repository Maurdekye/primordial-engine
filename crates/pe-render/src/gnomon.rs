//! The Gnomon, Gnosarium's dash (the user's pick D via @liaison-opus; design in
//! gnosarium-ux's dash-options.html, option D): a sundial. Pure CPU, like [`crate::menu`]: the images, where the
//! parts sit, and which part a ray hits. pe-app owns what the sections hold and what the buttons do.
//!
//! - **The dial:** a half-disc lying at about waist height, tilted toward the player, its straight edge (the
//!   diameter) away from them. The sections sit at the ends of its hour lines; the mic switch and Close sit at the two
//!   ends of the diameter. The dial is one textured quad, so its foreshortening is the scene's own.
//! - **The gnomon:** the dial's upright pointer, a thin gold right triangle standing on the diameter's centre. Its tip
//!   is the dash's grab point (a small diamond).
//! - **The page:** the chosen section, a [`MenuPanel`] standing upright on the gnomon at eye height, facing the player.
//! - **Desktop parity:** the same parts, flat in front of the camera, with the dial along the bottom of the view and
//!   the page above it ([`Arrangement::WINDOW`]).
//! - **Draw order:** overlays draw in list order with no depth test, so the dial goes first, then the gnomon, the page
//!   and the grip.
//! - **The look:** thin, steady gold lines over a faint lattice and dark frosted veils (the 10-31 approval). Colours
//!   are [`crate::menu`]'s.

use std::sync::Arc;

use glam::{DQuat, DVec3};
use pe_seams::Pos;

use crate::menu::{CHALK, Canvas, GOLD, INK, MenuPanel, PanelSpec, RayHit, VEIL, hit_quad};
use crate::pose::{WorldPose, offset_vec};
use crate::scene::{Overlay, OverlayImage};

// ---- drawing: lines and shapes on the menus' canvas -----------------------------------------------------------------

/// The distance from `p` to the segment `a`-`b`.
fn seg_dist(p: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let l2 = dx * dx + dy * dy;
    let t = if l2 > 0.0 { (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / l2).clamp(0.0, 1.0) } else { 0.0 };
    let (x, y) = (a[0] + t * dx - p[0], a[1] + t * dy - p[1]);
    (x * x + y * y).sqrt()
}

impl Canvas {
    /// A polyline `w` pixels wide, anti-aliased. Each pixel is drawn once, with the most any segment covers it, so
    /// the joints aren't doubled.
    pub(crate) fn stroke(&mut self, pts: &[[f32; 2]], closed: bool, w: f32, c: [f32; 3], a: f32) {
        let Some(&first) = pts.first() else { return };
        let pad = w / 2.0 + 1.0;
        let (mut lo, mut hi) = (first, first);
        for p in pts {
            lo = [lo[0].min(p[0]), lo[1].min(p[1])];
            hi = [hi[0].max(p[0]), hi[1].max(p[1])];
        }
        let clampx = |v: f32| v.clamp(0.0, self.size[0] as f32) as u32;
        let clampy = |v: f32| v.clamp(0.0, self.size[1] as f32) as u32;
        let (bx0, by0, bx1, by1) = (
            clampx((lo[0] - pad).floor()),
            clampy((lo[1] - pad).floor()),
            clampx((hi[0] + pad).ceil()),
            clampy((hi[1] + pad).ceil()),
        );
        if bx1 <= bx0 || by1 <= by0 {
            return;
        }
        let bw = (bx1 - bx0) as usize;
        let mut mask = vec![0f32; bw * (by1 - by0) as usize];
        let n = pts.len();
        let segs: Vec<([f32; 2], [f32; 2])> = if n == 1 {
            vec![(first, first)]
        } else {
            (0..if closed { n } else { n - 1 }).map(|i| (pts[i], pts[(i + 1) % n])).collect()
        };
        for (sa, sb) in segs {
            let x0 = clampx(sa[0].min(sb[0]) - pad).max(bx0);
            let x1 = clampx((sa[0].max(sb[0]) + pad).ceil()).min(bx1);
            let y0 = clampy(sa[1].min(sb[1]) - pad).max(by0);
            let y1 = clampy((sa[1].max(sb[1]) + pad).ceil()).min(by1);
            for y in y0..y1 {
                for x in x0..x1 {
                    let d = seg_dist([x as f32 + 0.5, y as f32 + 0.5], sa, sb);
                    let cov = (w / 2.0 + 0.5 - d).clamp(0.0, 1.0);
                    let m = &mut mask[(y - by0) as usize * bw + (x - bx0) as usize];
                    *m = m.max(cov);
                }
            }
        }
        for (k, m) in mask.into_iter().enumerate() {
            if m > 0.0 {
                self.over(bx0 + (k % bw) as u32, by0 + (k / bw) as u32, c, a * m);
            }
        }
    }

    /// A rectangle's edge: left, top, right, bottom.
    pub(crate) fn outline(&mut self, r: [f32; 4], w: f32, c: [f32; 3], a: f32) {
        self.stroke(&[[r[0], r[1]], [r[2], r[1]], [r[2], r[3]], [r[0], r[3]]], true, w, c, a);
    }

    /// A filled polygon, its edge anti-aliased by 2x2 samples a pixel.
    pub(crate) fn fill_poly(&mut self, pts: &[[f32; 2]], c: [f32; 3], a: f32) {
        let Some(&first) = pts.first() else { return };
        let (mut lo, mut hi) = (first, first);
        for p in pts {
            lo = [lo[0].min(p[0]), lo[1].min(p[1])];
            hi = [hi[0].max(p[0]), hi[1].max(p[1])];
        }
        let inside = |x: f32, y: f32| {
            let mut odd = false;
            for i in 0..pts.len() {
                let (p, q) = (pts[i], pts[(i + 1) % pts.len()]);
                if (p[1] > y) != (q[1] > y) && x < p[0] + (y - p[1]) / (q[1] - p[1]) * (q[0] - p[0]) {
                    odd = !odd;
                }
            }
            odd
        };
        let x0 = lo[0].floor().max(0.0) as u32;
        let y0 = lo[1].floor().max(0.0) as u32;
        let x1 = (hi[0].ceil().max(0.0) as u32).min(self.size[0]);
        let y1 = (hi[1].ceil().max(0.0) as u32).min(self.size[1]);
        for y in y0..y1 {
            for x in x0..x1 {
                let n = [(0.25, 0.25), (0.75, 0.25), (0.25, 0.75), (0.75, 0.75)]
                    .iter()
                    .filter(|(dx, dy)| inside(x as f32 + dx, y as f32 + dy))
                    .count();
                if n > 0 {
                    self.over(x, y, c, a * n as f32 / 4.0);
                }
            }
        }
    }

    /// A filled disc, its edge anti-aliased by its exact distance.
    pub(crate) fn fill_disc(&mut self, at: [f32; 2], r: f32, c: [f32; 3], a: f32) {
        let x0 = (at[0] - r - 1.0).floor().max(0.0) as u32;
        let y0 = (at[1] - r - 1.0).floor().max(0.0) as u32;
        let x1 = ((at[0] + r + 1.0).ceil().max(0.0) as u32).min(self.size[0]);
        let y1 = ((at[1] + r + 1.0).ceil().max(0.0) as u32).min(self.size[1]);
        for y in y0..y1 {
            for x in x0..x1 {
                let (dx, dy) = (x as f32 + 0.5 - at[0], y as f32 + 0.5 - at[1]);
                let cov = (r + 0.5 - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.over(x, y, c, a * cov);
                }
            }
        }
    }

    /// `text` centred on `at`, at most `max_w` wide.
    fn label(&mut self, at: [f32; 2], text: &str, px: f32, max_w: f32, c: [f32; 3]) {
        let r = [
            (at[0] - max_w / 2.0).max(0.0) as u32,
            (at[1] - px).max(0.0) as u32,
            ((at[0] + max_w / 2.0) as u32).min(self.size[0]),
            ((at[1] + px) as u32).min(self.size[1]),
        ];
        if r[2] > r[0] + 24 && r[3] > r[1] {
            self.text_centred(r, text, px, c);
        }
    }
}

/// Points on an ellipse's arc around `c` (`r` its two radii), from `a0` to `a1` degrees, in `n` steps. Angles run
/// clockwise on the image from +x: 90 is straight down.
fn arc(c: [f32; 2], r: [f32; 2], a0: f32, a1: f32, n: usize) -> Vec<[f32; 2]> {
    (0..=n)
        .map(|i| {
            let t = (a0 + (a1 - a0) * i as f32 / n as f32).to_radians();
            [c[0] + r[0] * t.cos(), c[1] + r[1] * t.sin()]
        })
        .collect()
}

/// A quadratic Bézier from `a` through control `b` to `c`.
fn quad(a: [f32; 2], b: [f32; 2], c: [f32; 2], n: usize) -> Vec<[f32; 2]> {
    (0..=n)
        .map(|i| {
            let t = i as f32 / n as f32;
            let u = 1.0 - t;
            [u * u * a[0] + 2.0 * u * t * b[0] + t * t * c[0], u * u * a[1] + 2.0 * u * t * b[1] + t * t * c[1]]
        })
        .collect()
}

// ---- the dial --------------------------------------------------------------------------------------------------------

/// A node's small drawing, after the mock-ups' glyphs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Glyph {
    Sessions,
    People,
    Inventory,
    Settings,
    Exit,
    Mic,
    MicOff,
    Close,
}

/// Where a node sits on the dial.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Place {
    /// At the end of the hour line this many degrees round from the diameter's right end (90: nearest the player).
    Hour(f32),
    /// The diameter's left end (the mic) or right end (Close).
    Left,
    Right,
}

/// One node on the dial: its caller's id, label and glyph.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub id: u32,
    pub label: String,
    pub glyph: Glyph,
    pub place: Place,
}

/// What the dial shows: its nodes, and which one's page is open.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DialSpec {
    pub nodes: Vec<Node>,
    pub selected: Option<u32>,
}

/// The dial's image in pixels, the diameter's centre in it, the dial's radius and a node's.
pub const DIAL_PX: [u32; 2] = [640, 384];
const DIAL_O: [f32; 2] = [320.0, 34.0];
const DIAL_R: f32 = 270.0;
const NODE_R: f32 = 26.0;
const LABEL_PX: f32 = 26.0;

/// A node's centre on the dial's image.
pub fn node_px(p: Place) -> [f32; 2] {
    match p {
        Place::Hour(t) => {
            let t = t.to_radians();
            [DIAL_O[0] + DIAL_R * t.cos(), DIAL_O[1] + DIAL_R * t.sin()]
        }
        Place::Left => [DIAL_O[0] - DIAL_R, DIAL_O[1]],
        Place::Right => [DIAL_O[0] + DIAL_R, DIAL_O[1]],
    }
}

/// The node at a point of the dial (`uv` as [`RayHit::uv`]): within its ring and a little beyond.
pub fn node_at(spec: &DialSpec, uv: [f64; 2]) -> Option<u32> {
    let (x, y) = ((uv[0] * DIAL_PX[0] as f64) as f32, (uv[1] * DIAL_PX[1] as f64) as f32);
    spec.nodes
        .iter()
        .map(|n| {
            let c = node_px(n.place);
            (n.id, ((x - c[0]).powi(2) + (y - c[1]).powi(2)).sqrt())
        })
        .filter(|(_, d)| *d <= NODE_R * 1.35)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(id, _)| id)
}

fn glyph(c: &mut Canvas, g: Glyph, at: [f32; 2], k: f32, col: [f32; 3]) {
    let p = |x: f32, y: f32| [at[0] + x * k, at[1] + y * k];
    let w = 2.0;
    let mut s = |pts: Vec<[f32; 2]>, closed: bool| c.stroke(&pts, closed, w, col, 0.95);
    let ring = |cx: f32, cy: f32, r: f32| arc(p(cx, cy), [r * k, r * k], 0.0, 360.0, 40);
    match g {
        Glyph::Sessions => {
            s(ring(0.0, 0.0, 6.0), true);
            let rot = (-20f32).to_radians();
            let e: Vec<[f32; 2]> = (0..=48)
                .map(|i| {
                    let t = (i as f32 / 48.0) * std::f32::consts::TAU;
                    let (x, y) = (11.5 * t.cos(), 3.6 * t.sin());
                    p(x * rot.cos() - y * rot.sin(), x * rot.sin() + y * rot.cos())
                })
                .collect();
            s(e, true);
        }
        Glyph::People => {
            s(ring(-4.5, -3.5, 3.4), true);
            s(ring(4.5, -3.5, 3.4), true);
            s(quad(p(-10.0, 8.0), p(-4.5, -1.0), p(1.0, 8.0), 16), false);
            s(quad(p(-1.0, 8.0), p(4.5, -1.0), p(10.0, 8.0), 16), false);
        }
        Glyph::Inventory => {
            s(vec![p(-7.5, -3.5), p(7.5, -3.5), p(7.5, 8.0), p(-7.5, 8.0)], true);
            s(quad(p(-4.0, -3.5), p(0.0, -11.0), p(4.0, -3.5), 16), false);
        }
        Glyph::Settings => {
            s(ring(0.0, 0.0, 4.2), true);
            for i in 0..8 {
                let a = i as f32 * std::f32::consts::FRAC_PI_4;
                s(vec![p(6.0 * a.cos(), 6.0 * a.sin()), p(9.5 * a.cos(), 9.5 * a.sin())], false);
            }
        }
        Glyph::Exit => {
            s(arc(p(0.0, 0.0), [7.5 * k, 7.5 * k], -48.0, 228.0, 40), false);
            s(vec![p(0.0, -9.5), p(0.0, -1.0)], false);
        }
        Glyph::Mic | Glyph::MicOff => {
            let mut body = arc(p(0.0, -6.0), [3.0 * k, 3.0 * k], 180.0, 360.0, 12);
            body.extend(arc(p(0.0, -1.0), [3.0 * k, 3.0 * k], 0.0, 180.0, 12));
            s(body, true);
            s(quad(p(-6.5, 0.0), p(0.0, 9.0), p(6.5, 0.0), 16), false);
            s(vec![p(0.0, 4.5), p(0.0, 9.0)], false);
            if g == Glyph::MicOff {
                s(vec![p(-8.0, -9.0), p(8.0, 9.0)], false);
            }
        }
        Glyph::Close => {
            s(vec![p(-6.0, -6.0), p(6.0, 6.0)], false);
            s(vec![p(-6.0, 6.0), p(6.0, -6.0)], false);
        }
    }
}

/// The dial's image with `hover` and `pressed` (node ids) shown.
pub fn dial_image(spec: &DialSpec, hover: Option<u32>, pressed: Option<u32>) -> OverlayImage {
    let mut c = Canvas { size: DIAL_PX, px: plate_px().to_vec() };
    draw_nodes(&mut c, spec, hover, pressed);
    c.image()
}

/// The dial's plate, which no state changes: its fills, lattice, arcs, diameter and foot, drawn once for the run
/// (@shader-reviewer's Medium on fd9cdd11: a dial state missed in the cache drew it all again).
fn plate_px() -> &'static [[f32; 4]] {
    static PX: std::sync::OnceLock<Vec<[f32; 4]>> = std::sync::OnceLock::new();
    PX.get_or_init(|| {
        let mut c = Canvas::new(DIAL_PX);
        draw_plate(&mut c);
        c.px
    })
}

fn draw_plate(c: &mut Canvas) {
    let (o, r) = (DIAL_O, DIAL_R);
    let mut plate = arc(o, [r, r], 0.0, 180.0, 96);
    plate.push(o);
    // The plate: a dark frosted half-disc with a faint gold wash, so the lines read against any world.
    c.fill_poly(&plate, VEIL.0, 0.55);
    c.fill_poly(&plate, GOLD, 0.05);
    // The lattice, inside the plate.
    let step = 30.0;
    let mut y = o[1] + step;
    while y < o[1] + r {
        let half = (r * r - (y - o[1]).powi(2)).sqrt();
        c.stroke(&[[o[0] - half, y], [o[0] + half, y]], false, 1.0, GOLD, 0.08);
        y += step;
    }
    let mut x = o[0] - r + step / 2.0 + (r % step);
    while x < o[0] + r {
        let down = (r * r - (x - o[0]).powi(2)).max(0.0).sqrt();
        c.stroke(&[[x, o[1]], [x, o[1] + down]], false, 1.0, GOLD, 0.08);
        x += step;
    }
    for k in [0.33, 0.66] {
        c.stroke(&arc(o, [r * k, r * k], 0.0, 180.0, 64), false, 1.2, GOLD, 0.45);
    }
    c.stroke(&arc(o, [r, r], 0.0, 180.0, 128), false, 1.8, GOLD, 0.9);
    c.stroke(&[[o[0] - r - 22.0, o[1]], [o[0] + r + 22.0, o[1]]], false, 1.8, GOLD, 0.9);
    // The gnomon's foot.
    c.fill_disc(o, 3.5, GOLD, 0.9);
}

/// What changes with the state: the hour lines (the open section's is brighter), the nodes and their labels.
fn draw_nodes(c: &mut Canvas, spec: &DialSpec, hover: Option<u32>, pressed: Option<u32>) {
    let o = DIAL_O;
    // The hour lines, to each section's ring.
    for n in &spec.nodes {
        if let Place::Hour(_) = n.place {
            let e = node_px(n.place);
            let (dx, dy) = (e[0] - o[0], e[1] - o[1]);
            let len = (dx * dx + dy * dy).sqrt();
            let end = [o[0] + dx * (len - NODE_R) / len, o[1] + dy * (len - NODE_R) / len];
            let sel = spec.selected == Some(n.id);
            c.stroke(&[o, end], false, if sel { 1.8 } else { 1.2 }, GOLD, if sel { 0.9 } else { 0.5 });
        }
    }
    for n in &spec.nodes {
        let at = node_px(n.place);
        let sel = spec.selected == Some(n.id);
        let ring = arc(at, [NODE_R, NODE_R], 0.0, 360.0, 48);
        c.fill_disc(at, NODE_R, INK, 0.92);
        let glow = if pressed == Some(n.id) {
            0.40
        } else if sel {
            0.24
        } else if hover == Some(n.id) {
            0.16
        } else {
            0.0
        };
        if glow > 0.0 {
            c.fill_disc(at, NODE_R - 4.0, GOLD, glow);
        }
        c.stroke(&ring, true, 1.8, GOLD, 0.9);
        if sel || hover == Some(n.id) {
            c.stroke(
                &arc(at, [NODE_R + 7.0, NODE_R + 7.0], 0.0, 360.0, 56),
                true,
                1.2,
                GOLD,
                if sel { 0.9 } else { 0.5 },
            );
        }
        glyph(c, n.glyph, at, 1.3, if sel { CHALK } else { GOLD });
        c.label([at[0], at[1] + NODE_R * 1.9], &n.label, LABEL_PX, 190.0, if sel { GOLD } else { CHALK });
    }
}

/// The gnomon's image: a right triangle, its upright edge on the image's left, its foot along the bottom.
const GNOMON_PX: [u32; 2] = [96, 480];
const GNOMON_FOOT: [f32; 2] = [6.0, 478.0];
const GNOMON_TIP: [f32; 2] = [6.0, 8.0];

fn gnomon_image() -> Arc<OverlayImage> {
    static IMG: std::sync::OnceLock<Arc<OverlayImage>> = std::sync::OnceLock::new();
    IMG.get_or_init(|| {
        let mut c = Canvas::new(GNOMON_PX);
        let far = [GNOMON_PX[0] as f32 - 4.0, GNOMON_FOOT[1]];
        c.fill_poly(&[GNOMON_FOOT, GNOMON_TIP, far], GOLD, 0.10);
        c.stroke(&[GNOMON_FOOT, GNOMON_TIP, far], false, 2.0, GOLD, 0.85);
        // The right angle at the foot.
        let (fx, fy) = (GNOMON_FOOT[0], GNOMON_FOOT[1]);
        c.stroke(&[[fx, fy - 14.0], [fx + 14.0, fy - 14.0], [fx + 14.0, fy]], false, 1.2, GOLD, 0.5);
        Arc::new(c.image())
    })
    .clone()
}

/// How the grip shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grip {
    Plain,
    Hover,
    Held,
}

fn grip_image(g: Grip) -> Arc<OverlayImage> {
    static IMG: std::sync::OnceLock<[Arc<OverlayImage>; 3]> = std::sync::OnceLock::new();
    let imgs = IMG.get_or_init(|| {
        [0.0, 0.35, 0.7].map(|glow| {
            let mut c = Canvas::new([48, 48]);
            let d = [[24.0, 3.0], [45.0, 24.0], [24.0, 45.0], [3.0, 24.0]];
            c.fill_poly(&d, INK, 0.92);
            if glow > 0.0 {
                c.fill_poly(&d, GOLD, glow);
            }
            c.stroke(&d, true, 2.2, GOLD, 0.95);
            c.fill_disc([24.0, 24.0], 4.0, GOLD, 0.9);
            Arc::new(c.image())
        })
    });
    imgs[match g {
        Grip::Plain => 0,
        Grip::Hover => 1,
        Grip::Held => 2,
    }]
    .clone()
}

// ---- the arrangement in the world ------------------------------------------------------------------------------------

/// Where the parts sit, in metres at a rig scale of 1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Arrangement {
    /// The dial's centre (on its diameter) ahead of the head along its level forward, and below it.
    pub ahead: f64,
    pub down: f64,
    /// The dial's plane, tipped back from upright by this many degrees (90 would lie flat).
    pub tilt_deg: f64,
    /// The gnomon's height, from the dial's centre to its tip.
    pub height: f64,
}

impl Arrangement {
    /// In VR: the dial at about waist height, the page at eye height [P, to tune in a headset].
    pub const VR: Arrangement = Arrangement { ahead: 0.5, down: 0.5, tilt_deg: 55.0, height: 0.62 };
    /// In the window: nearly flat in front of the camera, the dial along the bottom of a 1-radian view, the page
    /// above, clear of the help strip at the bottom and the fps counter at the top (the engine still of 11-27).
    pub const WINDOW: Arrangement = Arrangement { ahead: 0.85, down: 0.02, tilt_deg: 15.0, height: 0.35 };
}

/// Metres per dial pixel at scale 1: the dial's radius is 0.2 m.
pub const DIAL_M_PER_PX: f64 = 0.00075;
/// The grip's width, and the gap from the gnomon's tip down to the page's top, at scale 1.
const GRIP_M: f64 = 0.045;
const PAGE_GAP: f64 = 0.04;
/// The page stands this far in front of the gnomon, toward the player, so the gnomon is behind it.
const PAGE_FWD: f64 = 0.004;

/// The part of the dash a ray meets.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Part {
    /// The gnomon's tip: grab it to move the dash.
    Grip,
    /// The page, and its button there if any.
    Page(Option<u32>),
    /// A custom page's point that is no button ([`PageHit::Point`]).
    PageAt([f64; 2]),
    /// The dial, and its node there if any.
    Dial(Option<u32>),
}

/// What a page's point is: a button, or a point of its own (a map's spot), as uv across the page.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PageHit {
    Button(u32),
    Point([f64; 2]),
}

/// A page that draws itself, in place of the plain [`MenuPanel`] page: My Domain's map (@domain-eng), the
/// server browser's rows and keypad (@browser-eng). Each lives in its own file; the Gnomon places it on the gnomon,
/// hit-tests its quad and asks it what is under a point.
///
/// Its image should be cached per state, with any static layer drawn once: it is made on the frame thread.
pub trait PageView: Send {
    /// Its width and height in metres at a rig scale of 1.
    fn size_m(&self) -> [f64; 2];
    /// Its image with `hover` shown, and pressed while `pressed`.
    fn image(&mut self, hover: Option<PageHit>, pressed: bool) -> Arc<OverlayImage>;
    /// What is at `uv` (0..1 across from the left, 0..1 down from the top), if anything.
    fn hit(&self, uv: [f64; 2]) -> Option<PageHit>;
}

/// The point of a quad (`size_m`) where its image's pixel `px` (of `size_px`) is, in the quad's own plane.
fn px_local(px: [f32; 2], size_px: [u32; 2], size_m: [f64; 2]) -> DVec3 {
    DVec3::new(
        (px[0] as f64 / size_px[0] as f64 - 0.5) * size_m[0],
        (0.5 - px[1] as f64 / size_px[1] as f64) * size_m[1],
        0.0,
    )
}

/// A dial image as made for one hover and press of the cached content.
type Cached = (Option<u32>, Option<u32>, Arc<OverlayImage>);
const CACHED_STATES: usize = 16;

/// The dash in the world.
pub struct Gnomon {
    /// The dial's centre, on its diameter; upright, its +Z toward the player.
    pub anchor: WorldPose,
    pub scale: f64,
    pub arrangement: Arrangement,
    pub dial: DialSpec,
    /// The open section's page; its pose follows the anchor ([`Gnomon::set_page`], [`Gnomon::set_anchor`]).
    pub page: MenuPanel,
    /// A page that draws itself, shown instead of `page` while set ([`Gnomon::set_custom`]).
    custom: Option<Box<dyn PageView>>,
    /// The page's quad, whichever page it is.
    page_pose: WorldPose,
    cached_dial: Option<DialSpec>,
    cache: Vec<Cached>,
}

impl Gnomon {
    pub fn new(dial: DialSpec, page: PanelSpec, anchor: WorldPose, scale: f64, arrangement: Arrangement) -> Gnomon {
        let mut g = Gnomon {
            anchor,
            scale,
            arrangement,
            dial,
            page: MenuPanel::new(page, anchor, scale),
            custom: None,
            page_pose: anchor,
            cached_dial: None,
            cache: Vec::new(),
        };
        g.place_page();
        g
    }

    /// The anchor `rel` from the head in its level frame (x right, y up, -z ahead; metres at scale 1), turned to face
    /// the head.
    pub fn anchor_at(head: &WorldPose, rel: DVec3, scale: f64) -> WorldPose {
        let yaw = level_yaw(head.rot * DVec3::NEG_Z).unwrap_or(DQuat::IDENTITY);
        let pos = head.moved(yaw * rel * scale).pos;
        WorldPose { pos, rot: level_yaw(offset_vec(pos - head.pos)).unwrap_or(yaw) }
    }

    /// The default `rel` of an arrangement.
    pub fn default_rel(a: &Arrangement) -> DVec3 {
        DVec3::new(0.0, -a.down, -a.ahead)
    }

    /// `anchor`'s `rel` from `head` ([`Gnomon::anchor_at`]'s inverse).
    pub fn rel_of(head: &WorldPose, anchor: &WorldPose, scale: f64) -> DVec3 {
        let rot = level_yaw(head.rot * DVec3::NEG_Z).unwrap_or(DQuat::IDENTITY);
        rot.inverse() * offset_vec(anchor.pos - head.pos) / scale
    }

    pub fn set_anchor(&mut self, anchor: WorldPose, scale: f64) {
        (self.anchor, self.scale) = (anchor, scale);
        self.place_page();
    }

    pub fn set_page(&mut self, spec: PanelSpec) {
        if self.page.spec != spec {
            self.page.spec = spec;
            self.place_page();
        }
    }

    /// Show `view` as the page instead of the plain panel; `None` goes back to the panel.
    pub fn set_custom(&mut self, view: Option<Box<dyn PageView>>) {
        self.custom = view;
        self.place_page();
    }

    /// Whether a custom page is showing.
    pub fn has_custom(&self) -> bool {
        self.custom.is_some()
    }

    fn page_size(&self) -> [f64; 2] {
        match &self.custom {
            Some(v) => {
                // A custom page taller than the room between the tip and the foot is shrunk to fit, so it never
                // covers the dial (which would take the dial's clicks: the page is hit first).
                let s = v.size_m().map(|m| m * self.scale);
                let room = self.page_room();
                if s[1] > room { s.map(|m| m * room / s[1]) } else { s }
            }
            None => self.page.size(),
        }
    }

    /// The tallest page that fits under the tip with a gap above the foot, in metres.
    fn page_room(&self) -> f64 {
        (self.arrangement.height - 2.0 * PAGE_GAP) * self.scale
    }

    /// Move the dash so the gnomon's tip is at `tip`, turned to face `head`.
    pub fn move_tip_to(&mut self, tip: Pos, head: &WorldPose) {
        let up = DVec3::Y * self.arrangement.height * self.scale;
        let pos = WorldPose { pos: tip, rot: DQuat::IDENTITY }.moved(-up).pos;
        let rot = level_yaw(offset_vec(pos - head.pos)).unwrap_or(self.anchor.rot);
        self.set_anchor(WorldPose { pos, rot }, self.scale);
    }

    /// The gnomon's tip.
    pub fn tip(&self) -> Pos {
        self.anchor.moved(self.anchor.rot * DVec3::Y * self.arrangement.height * self.scale).pos
    }

    fn dial_rot(&self) -> DQuat {
        self.anchor.rot * DQuat::from_rotation_x(-self.arrangement.tilt_deg.to_radians())
    }

    fn dial_size(&self) -> [f64; 2] {
        [DIAL_PX[0] as f64 * DIAL_M_PER_PX * self.scale, DIAL_PX[1] as f64 * DIAL_M_PER_PX * self.scale]
    }

    /// The dial quad's centre and orientation.
    pub fn dial_pose(&self) -> WorldPose {
        let rot = self.dial_rot();
        let o = px_local(DIAL_O, DIAL_PX, self.dial_size());
        WorldPose { pos: self.anchor.moved(rot * -o).pos, rot }
    }

    fn gnomon_size(&self) -> [f64; 2] {
        let h = self.arrangement.height * self.scale * GNOMON_PX[1] as f64 / (GNOMON_FOOT[1] - GNOMON_TIP[1]) as f64;
        [h * GNOMON_PX[0] as f64 / GNOMON_PX[1] as f64, h]
    }

    fn gnomon_pose(&self) -> WorldPose {
        let foot = px_local(GNOMON_FOOT, GNOMON_PX, self.gnomon_size());
        WorldPose { pos: self.anchor.moved(self.anchor.rot * -foot).pos, rot: self.anchor.rot }
    }

    fn grip_pose(&self) -> WorldPose {
        WorldPose { pos: self.tip(), rot: self.anchor.rot }
    }

    fn grip_size(&self) -> [f64; 2] {
        [GRIP_M * self.scale; 2]
    }

    fn place_page(&mut self) {
        self.page.scale = self.scale;
        let h = self.page_size()[1];
        let up = self.arrangement.height * self.scale - PAGE_GAP * self.scale - h / 2.0;
        self.page_pose = WorldPose {
            pos: self.anchor.moved(self.anchor.rot * DVec3::new(0.0, up, PAGE_FWD * self.scale)).pos,
            rot: self.anchor.rot,
        };
        self.page.pose = self.page_pose;
    }

    /// Where a dial node's centre is in the world.
    pub fn node_point(&self, id: u32) -> Option<Pos> {
        let n = self.dial.nodes.iter().find(|n| n.id == id)?;
        let d = self.dial_pose();
        Some(d.moved(d.rot * px_local(node_px(n.place), DIAL_PX, self.dial_size())).pos)
    }

    /// Where a page button's centre is in the world.
    pub fn button_point(&self, id: u32) -> Option<Pos> {
        let l = crate::menu::layout(&self.page.spec);
        let r = self.page.spec.buttons.iter().zip(&l.buttons).find(|(b, _)| b.id == id)?.1;
        let c = [(r[0] + r[2]) as f32 / 2.0, (r[1] + r[3]) as f32 / 2.0];
        let p = self.page.pose;
        Some(p.moved(p.rot * px_local(c, l.size, self.page.size())).pos)
    }

    /// The part of the dash a ray meets first, and where.
    pub fn hit(&self, origin: Pos, dir: DVec3) -> Option<(Part, RayHit)> {
        let grip = hit_quad(&self.grip_pose(), self.grip_size(), origin, dir)
            .filter(|h| (h.uv[0] - 0.5).abs() + (h.uv[1] - 0.5).abs() <= 0.5)
            .map(|h| (Part::Grip, h));
        let page = hit_quad(&self.page_pose, self.page_size(), origin, dir).map(|h| {
            let part = match &self.custom {
                Some(v) => match v.hit(h.uv) {
                    Some(PageHit::Button(b)) => Part::Page(Some(b)),
                    Some(PageHit::Point(p)) => Part::PageAt(p),
                    None => Part::Page(None),
                },
                None => Part::Page(crate::menu::button_at(&self.page.spec, h.uv)),
            };
            (part, h)
        });
        let dial = hit_quad(&self.dial_pose(), self.dial_size(), origin, dir)
            .map(|h| (Part::Dial(node_at(&self.dial, h.uv)), h));
        // What is drawn on top wins, so a click goes to what the player sees (@shader-reviewer's Low on fd9cdd11):
        // the grip, then the page, then the dial.
        grip.or(page).or(dial)
    }

    /// This frame's overlays, in draw order: the dial, the gnomon, the page, the grip.
    pub fn overlays(
        &mut self,
        dial: (Option<u32>, Option<u32>),
        page: (Option<PageHit>, bool),
        grip: Grip,
    ) -> Vec<Overlay> {
        if self.cached_dial.as_ref() != Some(&self.dial) {
            self.cached_dial = Some(self.dial.clone());
            self.cache.clear();
        }
        let image = match self.cache.iter().find(|(h, p, _)| (*h, *p) == dial) {
            Some((_, _, img)) => img.clone(),
            None => {
                let img = Arc::new(dial_image(&self.dial, dial.0, dial.1));
                if self.cache.len() == CACHED_STATES {
                    self.cache.remove(0);
                }
                self.cache.push((dial.0, dial.1, img.clone()));
                img
            }
        };
        let f = |s: [f64; 2]| [s[0] as f32, s[1] as f32];
        vec![
            Overlay { image, pose: self.dial_pose(), size: f(self.dial_size()) },
            Overlay { image: gnomon_image(), pose: self.gnomon_pose(), size: f(self.gnomon_size()) },
            match self.custom.as_mut() {
                Some(v) => Overlay { image: v.image(page.0, page.1), pose: self.page_pose, size: f(self.page_size()) },
                None => {
                    let b = match page.0 {
                        Some(PageHit::Button(b)) => Some(b),
                        _ => None,
                    };
                    self.page.overlay(b, b.filter(|_| page.1))
                }
            },
            Overlay { image: grip_image(grip), pose: self.grip_pose(), size: f(self.grip_size()) },
        ]
    }
}

/// The upright turn whose -Z points along `dir`'s level part.
fn level_yaw(dir: DVec3) -> Option<DQuat> {
    let level = DVec3::new(dir.x, 0.0, dir.z).try_normalize()?;
    Some(DQuat::from_rotation_arc(DVec3::NEG_Z, level))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::menu::Button;
    use pe_seams::Offset;

    fn at(d: [f64; 3]) -> Pos {
        Pos::ORIGIN.checked_add(Offset(d)).unwrap()
    }

    fn dial() -> DialSpec {
        let n = |id, label: &str, glyph, place| Node { id, label: label.into(), glyph, place };
        DialSpec {
            nodes: vec![
                n(1, "Leave · Quit", Glyph::Exit, Place::Hour(150.0)),
                n(2, "Settings", Glyph::Settings, Place::Hour(120.0)),
                n(3, "Sessions", Glyph::Sessions, Place::Hour(90.0)),
                n(4, "My Domain", Glyph::Inventory, Place::Hour(60.0)),
                n(5, "People", Glyph::People, Place::Hour(30.0)),
                n(6, "Mic", Glyph::MicOff, Place::Left),
                n(7, "Close", Glyph::Close, Place::Right),
            ],
            selected: Some(3),
        }
    }

    fn page() -> PanelSpec {
        PanelSpec { title: Some("Sessions".into()), buttons: vec![Button::new(10, "Invite a friend")], lines: vec![] }
    }

    fn head() -> WorldPose {
        WorldPose { pos: at([0.0, 1.6, 0.0]), rot: DQuat::IDENTITY }
    }

    fn gnomon() -> Gnomon {
        let a = Arrangement::VR;
        Gnomon::new(dial(), page(), Gnomon::anchor_at(&head(), Gnomon::default_rel(&a), 1.0), 1.0, a)
    }

    fn uv_of(px: [f32; 2]) -> [f64; 2] {
        [px[0] as f64 / DIAL_PX[0] as f64, px[1] as f64 / DIAL_PX[1] as f64]
    }

    #[test]
    fn each_node_is_hit_at_its_place_and_nothing_between() {
        let d = dial();
        for n in &d.nodes {
            assert_eq!(node_at(&d, uv_of(node_px(n.place))), Some(n.id), "{}", n.label);
        }
        assert_eq!(node_at(&d, uv_of(DIAL_O)), None, "the gnomon's foot");
        assert_eq!(node_at(&d, uv_of(node_px(Place::Hour(75.0)))), None, "between two hours");
    }

    #[test]
    fn the_dial_lies_low_and_tilted_and_the_page_stands_at_eye_height() {
        let g = gnomon();
        // The anchor 0.5 m ahead and 0.5 m below the head; the tip 0.62 m above it.
        assert!((g.anchor.pos - at([0.0, 1.1, -0.5])).length() < 1e-9);
        assert!((g.tip() - at([0.0, 1.72, -0.5])).length() < 1e-9);
        // The dial's face looks up and back toward the player; its nodes are nearer the player than its diameter.
        let n = g.dial_pose().rot * DVec3::Z;
        assert!(n.y > 0.7 && n.z > 0.4, "{n:?}");
        let near =
            g.dial_pose().moved(g.dial_pose().rot * px_local(node_px(Place::Hour(90.0)), DIAL_PX, g.dial_size()));
        assert!((near.pos - Pos::ORIGIN).0[2] > -0.5 && (near.pos - Pos::ORIGIN).0[1] < 1.1, "{:?}", near.pos);
        // The page: upright, facing the player, under the tip, its top below it.
        let p = g.page.pose;
        let top = (p.pos - Pos::ORIGIN).0[1] + g.page.size()[1] / 2.0;
        assert!((top - (1.72 - PAGE_GAP)).abs() < 1e-9 && (p.pos - Pos::ORIGIN).0[2] > -0.5);
        assert!((p.rot * DVec3::Z - DVec3::Z).length() < 1e-9);
    }

    #[test]
    fn a_ray_names_the_part_it_meets() {
        let g = gnomon();
        let eye = head().pos;
        let toward = |p: Pos| offset_vec(p - eye).normalize();
        assert_eq!(g.hit(eye, toward(g.tip())).map(|h| h.0), Some(Part::Grip));
        assert_eq!(g.hit(eye, toward(g.page.pose.pos)).map(|h| h.0), Some(Part::Page(Some(10))));
        for n in &g.dial.nodes {
            let p = g.dial_pose().moved(g.dial_pose().rot * px_local(node_px(n.place), DIAL_PX, g.dial_size())).pos;
            assert_eq!(g.hit(eye, toward(p)).map(|h| h.0), Some(Part::Dial(Some(n.id))), "{}", n.label);
        }
        assert!(g.hit(eye, DVec3::Z).is_none(), "behind");
    }

    #[test]
    fn moving_the_tip_moves_everything_and_keeps_the_dash_facing_the_head() {
        let mut g = gnomon();
        let to = at([0.4, 1.5, -0.4]);
        g.move_tip_to(to, &head());
        assert!((g.tip() - to).length() < 1e-9);
        let face = g.anchor.rot * DVec3::Z;
        let back = offset_vec(head().pos - g.anchor.pos);
        let back = DVec3::new(back.x, 0.0, back.z).normalize();
        assert!((face - back).length() < 1e-9, "{face:?} {back:?}");
        // The anchor round-trips through rel.
        let rel = Gnomon::rel_of(&head(), &g.anchor, 1.0);
        assert!((Gnomon::anchor_at(&head(), rel, 1.0).pos - g.anchor.pos).length() < 1e-9);
    }

    #[test]
    fn the_dial_image_shows_hover_press_and_selection_and_is_cached() {
        let d = dial();
        let at_node = |img: &OverlayImage, id: u32| {
            let p = node_px(d.nodes.iter().find(|n| n.id == id).unwrap().place);
            // Inside the ring, off the glyph.
            let (x, y) = (p[0] as u32 + 15, p[1] as u32 + 12);
            img.rgba[((y * img.width + x) * 4) as usize]
        };
        let plain = dial_image(&d, None, None);
        let hover = dial_image(&d, Some(2), None);
        let press = dial_image(&d, Some(2), Some(2));
        assert!(at_node(&hover, 2) > at_node(&plain, 2), "hover glows");
        assert!(at_node(&press, 2) > at_node(&hover, 2), "a press glows more");
        assert!(at_node(&plain, 3) > at_node(&plain, 2), "the open section glows");
        let mut g = gnomon();
        let a = g.overlays((None, None), (None, false), Grip::Plain);
        assert_eq!(a.len(), 4);
        let b = g.overlays((None, None), (None, false), Grip::Hover);
        assert!(Arc::ptr_eq(&a[0].image, &b[0].image), "same dial state: the same image");
        assert!(!Arc::ptr_eq(&a[3].image, &b[3].image), "the grip shows its hover");
        g.dial.selected = Some(4);
        let c = g.overlays((None, None), (None, false), Grip::Plain);
        assert!(!Arc::ptr_eq(&a[0].image, &c[0].image), "a new selection: a new image");
    }

    #[test]
    fn the_window_arrangement_fits_a_one_radian_view() {
        let a = Arrangement::WINDOW;
        let g = Gnomon::new(dial(), page(), Gnomon::anchor_at(&head(), Gnomon::default_rel(&a), 1.0), 1.0, a);
        // In tan of the angle from the view's centre: the help strip covers below -0.38, the fps counter above 0.42.
        let (below, above) = (-0.36, 0.42);
        let eye = head().pos;
        let d = g.dial_pose();
        let corners = [[0.0, 0.0], [DIAL_PX[0] as f32, DIAL_PX[1] as f32], [0.0, DIAL_PX[1] as f32]];
        let mut pts: Vec<Pos> =
            corners.iter().map(|c| d.moved(d.rot * px_local(*c, DIAL_PX, g.dial_size())).pos).collect();
        pts.push(g.grip_pose().moved(DVec3::Y * GRIP_M / 2.0).pos);
        for p in pts {
            let v = offset_vec(p - eye);
            let t = v.y / -v.z;
            assert!(t > below && t < above, "{v:?} ({t}) is outside the clear band");
        }
    }

    /// A page that draws itself: a fixed image, a button in its top half, points in its bottom half.
    struct Fake(Arc<OverlayImage>);

    impl PageView for Fake {
        fn size_m(&self) -> [f64; 2] {
            [0.3, 0.2]
        }
        fn image(&mut self, _: Option<PageHit>, _: bool) -> Arc<OverlayImage> {
            self.0.clone()
        }
        fn hit(&self, uv: [f64; 2]) -> Option<PageHit> {
            Some(if uv[1] < 0.5 { PageHit::Button(9) } else { PageHit::Point(uv) })
        }
    }

    #[test]
    fn a_custom_page_stands_on_the_gnomon_and_names_its_buttons_and_points() {
        let mut g = gnomon();
        let img = Arc::new(OverlayImage { width: 1, height: 1, rgba: vec![0; 4] });
        g.set_custom(Some(Box::new(Fake(img.clone()))));
        let o = g.overlays((None, None), (None, false), Grip::Plain);
        assert!(Arc::ptr_eq(&o[2].image, &img) && o[2].size == [0.3, 0.2], "its image, at its size");
        let top = (o[2].pose.pos - Pos::ORIGIN).0[1] + 0.1;
        assert!((top - (1.72 - PAGE_GAP)).abs() < 1e-6, "its top under the tip");
        let eye = head().pos;
        let at = |dy: f64| offset_vec(o[2].pose.moved(DVec3::new(0.0, dy, 0.0)).pos - eye).normalize();
        assert_eq!(g.hit(eye, at(0.05)).map(|h| h.0), Some(Part::Page(Some(9))));
        assert!(matches!(g.hit(eye, at(-0.05)).map(|h| h.0), Some(Part::PageAt(p)) if p[1] > 0.5));
        g.set_custom(None);
        assert!(!g.has_custom());
        assert_eq!(g.hit(eye, offset_vec(g.page.pose.pos - eye).normalize()).map(|h| h.0), Some(Part::Page(Some(10))));
    }

    #[test]
    fn a_custom_page_too_tall_for_the_gnomon_is_shrunk_to_stay_above_the_foot() {
        let a = Arrangement::WINDOW;
        let mut g = Gnomon::new(dial(), page(), Gnomon::anchor_at(&head(), Gnomon::default_rel(&a), 1.0), 1.0, a);
        let img = Arc::new(OverlayImage { width: 1, height: 1, rgba: vec![0; 4] });
        struct Tall(Arc<OverlayImage>);
        impl PageView for Tall {
            fn size_m(&self) -> [f64; 2] {
                [0.36, 0.50]
            }
            fn image(&mut self, _: Option<PageHit>, _: bool) -> Arc<OverlayImage> {
                self.0.clone()
            }
            fn hit(&self, _: [f64; 2]) -> Option<PageHit> {
                Some(PageHit::Button(9))
            }
        }
        g.set_custom(Some(Box::new(Tall(img))));
        let o = g.overlays((None, None), (None, false), Grip::Plain);
        let room = a.height - 2.0 * PAGE_GAP;
        assert!((f64::from(o[2].size[1]) - room).abs() < 1e-6, "{:?}", o[2].size);
        assert!((f64::from(o[2].size[0]) / f64::from(o[2].size[1]) - 0.36 / 0.50).abs() < 1e-6, "its shape kept");
        let foot = g.anchor.pos;
        let bottom = (o[2].pose.pos - foot).0[1] - f64::from(o[2].size[1]) / 2.0;
        assert!(bottom >= PAGE_GAP - 1e-6, "its bottom stays above the foot: {bottom}");
        // A dial node is the dial's again.
        let eye = head().pos;
        for n in &g.dial.nodes {
            let at = g.node_point(n.id).unwrap();
            let hit = g.hit(eye, offset_vec(at - eye).normalize()).map(|h| h.0);
            assert!(matches!(hit, Some(Part::Dial(_))), "{:?}: {hit:?}", n.place);
        }
    }

    /// Writes the dash's images, over the mock-ups' dark background, to `$PE_GNOMON_PREVIEW/*.png` for a look by eye.
    #[test]
    #[ignore = "writes files; run by hand with PE_GNOMON_PREVIEW set"]
    fn preview() {
        let Ok(dir) = std::env::var("PE_GNOMON_PREVIEW") else { return };
        let save = |name: &str, img: &OverlayImage| {
            let mut rgb = Vec::with_capacity(img.rgba.len());
            for px in img.rgba.chunks(4) {
                let a = px[3] as f32 / 255.0;
                let bg = [13.0, 23.0, 36.0];
                rgb.extend((0..3).map(|k| (px[k] as f32 * a + bg[k] * (1.0 - a)).round() as u8));
                rgb.push(255);
            }
            let f = std::fs::File::create(format!("{dir}/{name}.png")).unwrap();
            let mut e = png::Encoder::new(std::io::BufWriter::new(f), img.width, img.height);
            e.set_color(png::ColorType::Rgba);
            e.write_header().unwrap().write_image_data(&rgb).unwrap();
        };
        save("dial", &dial_image(&dial(), Some(2), None));
        let mut ms: Vec<f64> = (0..20)
            .map(|i| {
                let t = std::time::Instant::now();
                std::hint::black_box(dial_image(&dial(), Some(1 + i % 7), None));
                t.elapsed().as_secs_f64() * 1e3
            })
            .collect();
        ms.sort_by(f64::total_cmp);
        println!("dial_image over 20 states: min {:.2} ms, median {:.2} ms, max {:.2} ms", ms[0], ms[10], ms[19]);
        save("gnomon", &gnomon_image());
        save("grip", &grip_image(Grip::Hover));
        let page = PanelSpec {
            title: Some("Sessions".into()),
            buttons: vec![
                Button::new(1, "Invite a friend"),
                Button::new(2, "Join a session"),
                Button::new(3, "New session"),
            ],
            lines: vec!["Your session. Invite valid for 52 min".into()],
        };
        save("page", &crate::menu::render(&page, Some(2), None));
    }
}

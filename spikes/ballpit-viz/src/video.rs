//! Scene building from recorded displays, overlays, composition and ffmpeg encoding.

use std::io::Write;
use std::process::{Child, Command, Stdio};

use pe_session_sim::ballpit_target_mm;

use crate::analyze::*;
use crate::overlay::*;
use crate::record::*;
use crate::render::*;

pub const FFMPEG: &str = "C:/ffmpeg/bin/ffmpeg.exe";
/// Frames a snapped ball stays tinted (fading).
const TINT_FRAMES: usize = 15;

fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

fn hsv(h: f32, s: f32, v: f32) -> [f32; 3] {
    let i = (h * 6.0).floor();
    let f = h * 6.0 - i;
    let (p, q, t) = (v * (1.0 - s), v * (1.0 - f * s), v * (1.0 - (1.0 - f) * s));
    let rgb = match i as i32 % 6 {
        0 => [v, t, p],
        1 => [q, v, p],
        2 => [p, v, t],
        3 => [p, q, v],
        4 => [t, p, v],
        _ => [v, p, q],
    };
    rgb.map(srgb_to_linear)
}

/// Ball colours by spawn position: 16 patches of 4x4 columns, lighter for the upper layer, so
/// displaced and mixed balls stay visible.
pub fn ball_colors() -> Vec<[f32; 3]> {
    (0..BALLS)
        .map(|i| {
            let (ix, iz, iy) = (i % 16, (i / 16) % 16, i / 256);
            let patch = (ix / 4) + 4 * (iz / 4);
            let h = (patch as f32 * 0.618_034).fract();
            if iy == 0 { hsv(h, 0.70, 0.80) } else { hsv(h, 0.45, 0.97) }
        })
        .collect()
}

/// Per-ball tint (0..1) per frame: 1 when the ball snapped in that frame, fading over TINT_FRAMES.
pub fn tints(snap: &[Vec<f32>]) -> Vec<Vec<f32>> {
    let mut last: Vec<Option<usize>> = vec![None; BALLS];
    snap.iter()
        .enumerate()
        .map(|(f, s)| {
            (0..BALLS)
                .map(|i| {
                    if s[i] > SNAP_M {
                        last[i] = Some(f);
                    }
                    last[i].map_or(0.0, |l| {
                        let age = f - l;
                        if age < TINT_FRAMES { 1.0 - age as f32 / TINT_FRAMES as f32 } else { 0.0 }
                    })
                })
                .collect()
        })
        .collect()
}

pub fn build_frame(shot: &Shot, tint: &[f32], colors: &[[f32; 3]], show_ghost: bool) -> Frame {
    let mut f = Frame::default();
    let lin = |c: [f32; 3]| c.map(srgb_to_linear);
    f.boxes.push(Instance::cuboid([0.0, -0.5, 0.0], [2.0, 0.5, 2.0], { let c = lin([0.30, 0.31, 0.34]); [c[0], c[1], c[2], 1.0] }));
    let (w, h) = (1.2f32, 0.6f32);
    let glass = { let c = lin([0.62, 0.74, 0.92]); [c[0], c[1], c[2], 0.20] };
    f.glass_boxes.push(Instance::cuboid([w + 0.1, h, 0.0], [0.1, h, w + 0.2], glass));
    f.glass_boxes.push(Instance::cuboid([-w - 0.1, h, 0.0], [0.1, h, w + 0.2], glass));
    f.glass_boxes.push(Instance::cuboid([0.0, h, w + 0.1], [w + 0.2, h, 0.1], glass));
    f.glass_boxes.push(Instance::cuboid([0.0, h, -w - 0.1], [w + 0.2, h, 0.1], glass));
    let snap_c = lin([1.0, 0.08, 0.85]);
    for i in 0..BALLS {
        let t = tint[i];
        let c = colors[i];
        let c = [c[0] + (snap_c[0] - c[0]) * t, c[1] + (snap_c[1] - c[1]) * t, c[2] + (snap_c[2] - c[2]) * t];
        f.spheres.push(Instance::sphere(shot.pos[i], 0.06, [c[0], c[1], c[2], 1.0]));
    }
    for (obj, col) in [(ENGAGER_HAND, lin([1.0, 0.50, 0.08])), (ENGAGER_HAND + 1, lin([0.45, 0.45, 0.48]))] {
        let p = shot.pos[obj];
        f.spheres.push(Instance::sphere(p, 0.09, [col[0], col[1], col[2], 1.0]));
        let top = 2.6f32;
        if p[1] + 0.09 < top {
            let half = (top - p[1] - 0.09) / 2.0;
            f.boxes.push(Instance::cuboid([p[0], p[1] + 0.09 + half, p[2]], [0.02, half, 0.02], [col[0] * 0.7, col[1] * 0.7, col[2] * 0.7, 1.0]));
        }
    }
    if show_ghost {
        if let Some(t) = ballpit_target_mm(HZ, 1, shot.k) {
            let p = [t[0] as f32 / 1000.0, t[1] as f32 / 1000.0, t[2] as f32 / 1000.0];
            f.glass_spheres.push(Instance::sphere(p, 0.10, [1.0, 1.0, 1.0, 0.38]));
        }
    }
    f
}

pub fn spawn_ffmpeg(w: usize, h: usize, out: &str, crf: u32) -> Child {
    Command::new(FFMPEG)
        .args(["-y", "-loglevel", "error", "-f", "rawvideo", "-pix_fmt", "rgba", "-s", &format!("{w}x{h}"), "-r", "60", "-i", "-"])
        .args(["-c:v", "libx264", "-preset", "medium", "-crf", &crf.to_string(), "-pix_fmt", "yuv420p", "-movflags", "+faststart", out])
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn ffmpeg")
}

pub fn finish(mut c: Child) {
    drop(c.stdin.take());
    let st = c.wait().expect("ffmpeg");
    assert!(st.success(), "ffmpeg failed");
}

fn blit(dst: &mut [u8], dw: usize, src: &[u8], sw: usize, sh: usize, x0: usize, y0: usize) {
    for y in 0..sh {
        let d = ((y0 + y) * dw + x0) * 4;
        dst[d..d + sw * 4].copy_from_slice(&src[y * sw * 4..(y + 1) * sw * 4]);
    }
}

pub fn phase(k: u64) -> (String, [u8; 3]) {
    let t = (k * DT_US + EPOCH_US) as f64 / 1e6;
    if k < 120 {
        (format!("t {t:5.2} s  BEFORE the gesture"), [170, 200, 255])
    } else if k <= 340 {
        (format!("t {t:5.2} s  DURING the gesture"), [255, 200, 90])
    } else {
        (format!("t {t:5.2} s  AFTER the gesture (+{:.2} s)", (k - 340) as f64 / HZ as f64), [150, 235, 150])
    }
}

fn kb(b: f64) -> String {
    if b >= 1e6 { format!("{:.2} MB/s", b / 1e6) } else { format!("{:.1} kB/s", b / 1e3) }
}

fn mb(b: u64) -> String {
    if b >= 1_000_000 { format!("{:.1} MB", b as f64 / 1e6) } else { format!("{:.0} kB", b as f64 / 1e3) }
}

fn d3(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

fn hand_vs_controller(s: &Shot) -> Option<f32> {
    ballpit_target_mm(HZ, 1, s.k).map(|t| d3(s.pos[ENGAGER_HAND], [t[0] as f32 / 1000.0, t[1] as f32 / 1000.0, t[2] as f32 / 1000.0]))
}

fn timeline(img: &mut Img, k: u64) {
    let (w, h) = (img.w as i32, img.h as i32);
    let y0 = h - 7;
    img.rect(0, y0, w, h, [20, 20, 26], 0.85);
    let x = |kk: u64| ((kk - K0) as f64 / (K1 - K0) as f64 * w as f64) as i32;
    img.rect(x(120), y0 + 1, x(341), h - 1, [200, 150, 60], 0.9);
    img.rect(x(k) - 1, y0 - 3, x(k) + 2, h, [255, 255, 255], 1.0);
}

/// A small time-series graph of `vals` (one per recorded tick) up to tick index `f`, on a fixed
/// y scale so cells compare directly. Values above the scale are drawn red at full height.
#[allow(clippy::too_many_arguments)]
fn graph(img: &mut Img, text: &Text, x0: i32, y0: i32, w: i32, h: i32, vals: &[f32], f: usize, ymax: f32, label: &str, c: [u8; 3], px: f32) {
    text.draw(img, x0 as f32, y0 as f32, px, label, [185, 190, 200]);
    let gy = y0 + (px * 1.25) as i32;
    img.rect(x0, gy, x0 + w, gy + h, [30, 33, 42], 1.0);
    let n = vals.len().max(2);
    let sx = |i: usize| x0 + (i as f32 / (n - 1) as f32 * (w - 1) as f32) as i32;
    // The gesture window.
    let (g0, g1) = ((120 - K0) as usize, (340 - K0) as usize);
    img.rect(sx(g0), gy + h - 2, sx(g1.min(n - 1)), gy + h, [200, 150, 60], 1.0);
    for (i, v) in vals.iter().enumerate().take(f + 1) {
        let hh = ((v / ymax).min(1.0) * (h - 2) as f32).round() as i32;
        let x = sx(i);
        let clipped = *v > ymax;
        img.rect(x, gy + h - 2 - hh, x + 1, gy + h - 2, if clipped { [255, 90, 90] } else { c }, 1.0);
    }
    img.rect(sx(f), gy, sx(f) + 1, gy + h, [255, 255, 255], 0.8);
}

/// Text lines for one view's side column: (text, colour, size factor).
fn lines(cell: &Cell, a: &Analysis, r: &Recording, f: usize, observer: bool, compact: bool) -> Vec<(String, [u8; 3], f32)> {
    let shot = if observer { &r.obs[f] } else { &r.eng[f] };
    let d = if observer { a.obs[f] } else { a.eng[f] };
    let m = shot.m;
    let white = [235, 235, 240];
    let dim = [185, 190, 200];
    let naive = cell.mode == Mode::Naive;
    let mut v: Vec<(String, [u8; 3], f32)> = Vec::new();
    if compact && !observer {
        v.push((short_net(cell), [120, 200, 255], 1.25));
    } else if compact {
        v.push((String::new(), dim, 1.25));
    }
    v.push((if observer { "OBSERVER (member 2)".into() } else { "ENGAGER (member 1)".into() }, white, 1.2));
    if !compact {
        v.push(("what it displays now".into(), dim, 1.0));
        let (ph, pc) = phase(shot.k);
        v.push((ph, pc, 1.0));
    }
    v.push((String::new(), dim, 0.5));
    if observer {
        let col = if d.div_count == 0 { [150, 235, 150] } else { [255, 210, 120] };
        v.push(("vs the engager right now:".into(), dim, 1.0));
        v.push((format!("  {} balls off by >1 cm", d.div_count), col, 1.0));
        v.push((format!("  worst ball {:.1} cm", d.div_max_m * 100.0), col, 1.0));
        v.push((format!("  hand {:.1} cm apart", d.hand_div_m * 100.0), white, 1.0));
    } else {
        match hand_vs_controller(shot) {
            Some(x) => v.push((format!("hand vs controller {:.1} cm", x * 100.0), white, 1.0)),
            None => v.push(("hand vs controller: idle".into(), white, 1.0)),
        }
        if let Mode::Pose { ahead } = cell.mode {
            if ahead > 0 {
                v.push((format!("(input delay +{:.0} ms by design)", ahead as f64 * 1000.0 / HZ as f64), [255, 180, 120], 1.0));
            }
        }
    }
    v.push((format!("ball snaps {:.0}/s", d.snaps_s), if d.snaps_s > 0.0 { [255, 120, 230] } else { dim }, 1.0));
    v.push((String::new(), dim, 0.5));
    if naive && observer {
        v.push(("no simulation: draws the".into(), dim, 1.0));
        v.push(("newest state received".into(), dim, 1.0));
    } else {
        v.push((format!("rollbacks {:.0}/s", m.rollbacks_s), dim, 1.0));
        v.push((format!("re-sim CPU {:.2} ms/frame", m.resim_ms), dim, 1.0));
    }
    v.push((format!("tx {}  rx {}", kb(m.tx_bps), kb(m.rx_bps)), dim, 1.0));
    v.push((format!("queue {:.0} ms  q-drops {}", m.queue_ms, m.cap_drops), if m.queue_ms > 50.0 { [255, 150, 150] } else { dim }, 1.0));
    if naive && !observer {
        v.push(("snapshots: none needed".into(), dim, 1.0));
    } else if naive {
        v.push((format!("state table {}", mb(m.snapshot_bytes)), dim, 1.0));
    } else {
        v.push((format!("snapshots {} ({} kf)", mb(m.snapshot_bytes), m.snapshots), dim, 1.0));
        if !compact {
            v.push((format!("confirmed lag {} frames", m.conf_lag), dim, 1.0));
        }
    }
    if m.stalled && !naive {
        v.push(("SIM STALLED (spec. limit)".into(), [255, 110, 110], 1.0));
    }
    v
}

fn short_net(c: &Cell) -> String {
    match c.cap_kbps {
        None => format!("{} ms | unlimited", c.latency_ms),
        Some(k) => format!("{} ms | {k} kbit/s", c.latency_ms),
    }
}

pub struct Prepared {
    pub tints_e: Vec<Vec<f32>>,
    pub tints_o: Vec<Vec<f32>>,
}

pub fn prepare(a: &Analysis) -> Prepared {
    Prepared { tints_e: tints(&a.eng_snap), tints_o: tints(&a.obs_snap) }
}

/// One peer's view: a text column (width `tw`) left of the 3D render (width `w - tw`).
#[allow(clippy::too_many_arguments)]
pub fn view(rd: &mut Renderer, text: &Text, colors: &[[f32; 3]], r: &Recording, a: &Analysis, p: &Prepared, f: usize, observer: bool, w: usize, tw: usize, h: usize, compact: bool) -> Vec<u8> {
    let (shot, tint) = if observer { (&r.obs[f], &p.tints_o[f]) } else { (&r.eng[f], &p.tints_e[f]) };
    let fr = build_frame(shot, tint, colors, true);
    let w3 = w - tw;
    let mut px3 = rd.render(&fr, w3 as u32, h as u32);
    {
        let mut img3 = Img { px: &mut px3, w: w3, h };
        timeline(&mut img3, shot.k);
    }
    let mut px = vec![0u8; w * h * 4];
    for c in px.chunks_exact_mut(4) {
        c.copy_from_slice(&[16, 18, 24, 255]);
    }
    blit(&mut px, w, &px3, w3, h, tw, 0);
    let mut img = Img { px: &mut px, w, h };
    let fs = if compact { 12.5 } else { 16.0 };
    let mut y = 6.0;
    for (s, c, k) in lines(&r.cell, a, r, f, observer, compact) {
        text.draw(&mut img, 8.0, y, fs * k, &s, c);
        y += fs * k * 1.22;
    }
    // Graphs at the bottom of the column, on fixed scales.
    let gh = if compact { 26 } else { 46 };
    let gw = tw as i32 - 16;
    let step = gh + (fs * 1.25) as i32 + 6;
    let mut gy = h as i32 - 12 - step * if observer { 2 } else { 1 };
    let snaps: Vec<f32> = (if observer { &a.obs } else { &a.eng }).iter().map(|d| d.snaps_s as f32).collect();
    if observer {
        let div: Vec<f32> = a.obs.iter().map(|d| d.div_count as f32).collect();
        graph(&mut img, text, 8, gy, gw, gh, &div, f, 250.0, "balls >1 cm off (0-250)", [255, 210, 120], fs * 0.85);
        gy += step;
    }
    graph(&mut img, text, 8, gy, gw, gh, &snaps, f, 3000.0, "ball snaps/s (0-3000)", [255, 120, 230], fs * 0.85);
    img.rect(w as i32 - 1, 0, w as i32, h as i32, [0, 0, 0], 1.0);
    px
}

pub const VW: usize = 960;
pub const TW: usize = 320;
pub const VH: usize = 540;
pub const HDR: usize = 36;

pub fn cell_video(rd: &mut Renderer, text: &Text, colors: &[[f32; 3]], r: &Recording, a: &Analysis, out: &str) {
    let p = prepare(a);
    let (w, h) = (2 * VW, VH + HDR);
    let mut ff = spawn_ffmpeg(w, h, out, 20);
    let stdin = ff.stdin.as_mut().unwrap();
    let n = r.eng.len().min(r.obs.len());
    let mut frame = vec![0u8; w * h * 4];
    for f in 0..n {
        frame.fill(0);
        for px in frame.chunks_exact_mut(4) {
            px.copy_from_slice(&[16, 18, 24, 255]);
        }
        {
            let mut img = Img { px: &mut frame, w, h };
            let title = format!("{}   |   {}", r.cell.mode_label(), r.cell.net_label());
            text.draw(&mut img, 10.0, 8.0, 20.0, &title, [255, 255, 255]);
            let legend = "magenta = ball snapped (correction)   white ghost = engager's controller now";
            let lw = text.width(14.0, legend);
            text.draw(&mut img, w as f32 - lw - 12.0, 12.0, 14.0, legend, [180, 185, 195]);
        }
        let e = view(rd, text, colors, r, a, &p, f, false, VW, TW, VH, false);
        let o = view(rd, text, colors, r, a, &p, f, true, VW, TW, VH, false);
        blit(&mut frame, w, &e, VW, VH, 0, HDR);
        blit(&mut frame, w, &o, VW, VH, VW, HDR);
        stdin.write_all(&frame).expect("ffmpeg pipe");
    }
    finish(ff);
}

pub const GW: usize = 640;
pub const GH: usize = 352;
pub const GTW: usize = 256;
pub const GHDR: usize = 48;

/// The matrix grid: rows = network conditions, columns = configurations; each cell is the
/// engager and observer side by side.
pub fn grid_video(rd: &mut Renderer, text: &Text, colors: &[[f32; 3]], recs: &[(Recording, Analysis)], out: &str) {
    let modes: Vec<Mode> = {
        let mut v: Vec<Mode> = Vec::new();
        for (r, _) in recs {
            if !v.contains(&r.cell.mode) {
                v.push(r.cell.mode);
            }
        }
        v
    };
    let nets: Vec<(u64, Option<u64>)> = {
        let mut v = Vec::new();
        for (r, _) in recs {
            let k = (r.cell.latency_ms, r.cell.cap_kbps);
            if !v.contains(&k) {
                v.push(k);
            }
        }
        v
    };
    let (w, h) = (modes.len() * 2 * GW, GHDR + nets.len() * GH);
    let preps: Vec<Prepared> = recs.iter().map(|(_, a)| prepare(a)).collect();
    let mut ff = spawn_ffmpeg(w, h, out, 22);
    let n = recs.iter().map(|(r, _)| r.eng.len().min(r.obs.len())).min().unwrap();
    let mut frame = vec![0u8; w * h * 4];
    for f in 0..n {
        for px in frame.chunks_exact_mut(4) {
            px.copy_from_slice(&[16, 18, 24, 255]);
        }
        {
            let mut img = Img { px: &mut frame, w, h };
            let (ph, pc) = phase(recs[0].0.eng[f].k);
            text.draw(&mut img, 12.0, 2.0, 20.0, &ph, pc);
            let legend = "each cell: ENGAGER (left) | OBSERVER (right)   magenta = snapped ball   white ghost = engager's controller";
            let lw = text.width(15.0, legend);
            text.draw(&mut img, w as f32 - lw - 12.0, 4.0, 15.0, legend, [180, 185, 195]);
            for (ci, m) in modes.iter().enumerate() {
                let label = recs.iter().find(|(r, _)| r.cell.mode == *m).unwrap().0.cell.mode_label();
                let lw = text.width(20.0, &label);
                let cx = (ci * 2 * GW + GW) as f32;
                text.draw(&mut img, cx - lw / 2.0, 24.0, 20.0, &label, [255, 255, 255]);
                img.rect((ci * 2 * GW) as i32, 24, (ci * 2 * GW) as i32 + 3, h as i32, [70, 75, 90], 1.0);
            }
        }
        for (i, (r, a)) in recs.iter().enumerate() {
            let ci = modes.iter().position(|m| *m == r.cell.mode).unwrap();
            let ri = nets.iter().position(|k| *k == (r.cell.latency_ms, r.cell.cap_kbps)).unwrap();
            let e = view(rd, text, colors, r, a, &preps[i], f, false, GW, GTW, GH, true);
            let o = view(rd, text, colors, r, a, &preps[i], f, true, GW, GTW, GH, true);
            let (x0, y0) = (ci * 2 * GW, GHDR + ri * GH);
            blit(&mut frame, w, &e, GW, GH, x0, y0);
            blit(&mut frame, w, &o, GW, GH, x0 + GW, y0);
            let mut img = Img { px: &mut frame, w, h };
            img.rect(x0 as i32, y0 as i32, (x0 + 2 * GW) as i32, y0 as i32 + 2, [70, 75, 90], 1.0);
            if ci > 0 {
                img.rect(x0 as i32, y0 as i32, x0 as i32 + 3, (y0 + GH) as i32, [70, 75, 90], 1.0);
            }
        }
        ff.stdin.as_mut().unwrap().write_all(&frame).expect("ffmpeg pipe");
    }
    finish(ff);
}

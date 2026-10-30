//! CPU text and panel drawing on RGBA8 frames (ab_glyph, a system TrueType font).

use ab_glyph::{point, Font, FontVec, PxScale, ScaleFont};

pub struct Img<'a> {
    pub px: &'a mut [u8],
    pub w: usize,
    pub h: usize,
}

impl Img<'_> {
    fn blend(&mut self, x: i32, y: i32, c: [u8; 3], a: f32) {
        if x < 0 || y < 0 || x as usize >= self.w || y as usize >= self.h || a <= 0.0 {
            return;
        }
        let i = (y as usize * self.w + x as usize) * 4;
        for k in 0..3 {
            let d = self.px[i + k] as f32;
            self.px[i + k] = (d + (c[k] as f32 - d) * a.min(1.0)).round() as u8;
        }
    }

    pub fn rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, c: [u8; 3], a: f32) {
        for y in y0.max(0)..y1.min(self.h as i32) {
            for x in x0.max(0)..x1.min(self.w as i32) {
                self.blend(x, y, c, a);
            }
        }
    }
}

pub struct Text {
    font: FontVec,
}

impl Text {
    pub fn load() -> Self {
        for p in ["C:/Windows/Fonts/consola.ttf", "C:/Windows/Fonts/arial.ttf", "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"] {
            if let Ok(b) = std::fs::read(p) {
                if let Ok(font) = FontVec::try_from_vec(b) {
                    return Text { font };
                }
            }
        }
        panic!("no usable TrueType font found");
    }

    pub fn width(&self, px: f32, s: &str) -> f32 {
        let f = self.font.as_scaled(PxScale::from(px));
        s.chars().map(|c| f.h_advance(f.glyph_id(c))).sum()
    }

    /// Draw `s` with its top-left corner at (x, y); returns the advance width.
    pub fn draw(&self, img: &mut Img, x: f32, y: f32, px: f32, s: &str, c: [u8; 3]) -> f32 {
        let f = self.font.as_scaled(PxScale::from(px));
        let mut cx = x;
        let base = y + f.ascent();
        for ch in s.chars() {
            let id = f.glyph_id(ch);
            let g = id.with_scale_and_position(PxScale::from(px), point(cx, base));
            if let Some(o) = self.font.outline_glyph(g) {
                let b = o.px_bounds();
                o.draw(|gx, gy, cov| img.blend(b.min.x as i32 + gx as i32, b.min.y as i32 + gy as i32, c, cov));
            }
            cx += f.h_advance(id);
        }
        cx - x
    }
}

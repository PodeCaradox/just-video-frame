//! A small RGBA (sRGB-encoded) software canvas with text, for the headset UI.

use std::collections::HashMap;

pub type Rgb = [u8; 3];

pub struct Canvas {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl Canvas {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            pixels: vec![0; (width * height * 4) as usize],
        }
    }

    pub fn clear(&mut self, color: Rgb) {
        for px in self.pixels.chunks_exact_mut(4) {
            px.copy_from_slice(&[color[0], color[1], color[2], 255]);
        }
    }

    /// Blends `color` at coverage `alpha` (0..=1) into pixel (x, y).
    #[inline]
    fn blend(&mut self, x: i32, y: i32, color: Rgb, alpha: f32) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 || alpha <= 0.0 {
            return;
        }
        let i = ((y as u32 * self.width + x as u32) * 4) as usize;
        let a = alpha.min(1.0);
        for (dst, src) in self.pixels[i..i + 3].iter_mut().zip(color) {
            *dst = (*dst as f32 + (src as f32 - *dst as f32) * a).round() as u8;
        }
        self.pixels[i + 3] = 255;
    }

    /// Filled rectangle with anti-aliased rounded corners.
    pub fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, color: Rgb) {
        let r = radius.min(w / 2.0).min(h / 2.0).max(0.0);
        let (x0, y0) = (x.floor() as i32, y.floor() as i32);
        let (x1, y1) = ((x + w).ceil() as i32, (y + h).ceil() as i32);
        for py in y0..y1 {
            for px in x0..x1 {
                let (cx, cy) = (px as f32 + 0.5, py as f32 + 0.5);
                // Signed distance to the rounded rectangle.
                let qx = (cx - (x + w / 2.0)).abs() - (w / 2.0 - r);
                let qy = (cy - (y + h / 2.0)).abs() - (h / 2.0 - r);
                let outside =
                    (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r;
                self.blend(px, py, color, 0.5 - outside);
            }
        }
    }

    /// A see-through rounded rectangle on a transparent canvas, written as
    /// premultiplied alpha (what OpenXR's alpha-blended layers expect).
    #[allow(clippy::too_many_arguments)]
    pub fn translucent_rect(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius: f32,
        color: Rgb,
        opacity: f32,
    ) {
        let r = radius.min(w / 2.0).min(h / 2.0).max(0.0);
        let (x0, y0) = (x.floor().max(0.0) as i32, y.floor().max(0.0) as i32);
        let (x1, y1) = (
            ((x + w).ceil() as i32).min(self.width as i32),
            ((y + h).ceil() as i32).min(self.height as i32),
        );
        for py in y0..y1 {
            for px in x0..x1 {
                let (cx, cy) = (px as f32 + 0.5, py as f32 + 0.5);
                let qx = (cx - (x + w / 2.0)).abs() - (w / 2.0 - r);
                let qy = (cy - (y + h / 2.0)).abs() - (h / 2.0 - r);
                let outside =
                    (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r;
                let a = (0.5 - outside).clamp(0.0, 1.0) * opacity;
                if a <= 0.0 {
                    continue;
                }
                let i = ((py as u32 * self.width + px as u32) * 4) as usize;
                for (k, c) in color.iter().enumerate() {
                    let dst = self.pixels[i + k] as f32;
                    self.pixels[i + k] = (*c as f32 * a + dst * (1.0 - a)).round() as u8;
                }
                let dst = self.pixels[i + 3] as f32 / 255.0;
                self.pixels[i + 3] = ((a + dst * (1.0 - a)) * 255.0).round() as u8;
            }
        }
    }

    pub fn circle(&mut self, cx: f32, cy: f32, radius: f32, color: Rgb) {
        self.rect(
            cx - radius,
            cy - radius,
            radius * 2.0,
            radius * 2.0,
            radius,
            color,
        );
    }

    /// Draws an opaque RGBA image at (x, y) with rounded corners: rows are
    /// copied straight, and only the corner pixels are blended (anti-aliased).
    pub fn image(&mut self, x: i32, y: i32, image: &crate::media::Thumb, radius: f32) {
        let (w, h) = (image.width as i32, image.height as i32);
        let r = radius.min(w as f32 / 2.0).min(h as f32 / 2.0).max(0.0);
        let band = r.ceil() as i32;
        let (cw, ch) = (self.width as i32, self.height as i32);
        for sy in 0..h {
            let py = y + sy;
            if py < 0 || py >= ch {
                continue;
            }
            let corner = sy < band || sy >= h - band;
            let (from, to) = if corner { (band, w - band) } else { (0, w) };
            let (from, to) = (from.max(-x), to.min(cw - x));
            let src = &image.rgba[(sy * w * 4) as usize..((sy + 1) * w * 4) as usize];
            if from < to {
                let dst = ((py * cw + x + from) * 4) as usize;
                let n = ((to - from) * 4) as usize;
                self.pixels[dst..dst + n].copy_from_slice(&src[(from * 4) as usize..][..n]);
            }
            if !corner {
                continue;
            }
            // Distance from the corner circle's centre decides the coverage.
            let dy = if sy < band {
                r - (sy as f32 + 0.5)
            } else {
                sy as f32 + 0.5 - (h as f32 - r)
            };
            for sx in (0..band).chain(w - band..w) {
                let dx = if sx < band {
                    r - (sx as f32 + 0.5)
                } else {
                    sx as f32 + 0.5 - (w as f32 - r)
                };
                let a =
                    (r + 0.5 - (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt()).clamp(0.0, 1.0);
                let px = x + sx;
                if a > 0.0 && px >= 0 && px < cw {
                    let i = (sx * 4) as usize;
                    self.blend(px, py, [src[i], src[i + 1], src[i + 2]], a);
                }
            }
        }
    }

    /// Copies another canvas to (x, y) (no scaling).
    pub fn blit(&mut self, src: &Canvas, x: i32, y: i32) {
        for sy in 0..src.height as i32 {
            for sx in 0..src.width as i32 {
                let i = ((sy as u32 * src.width + sx as u32) * 4) as usize;
                let p = &src.pixels[i..i + 4];
                self.blend(x + sx, y + sy, [p[0], p[1], p[2]], p[3] as f32 / 255.0);
            }
        }
    }
}

/// Noto Sans from the system, with Noto Sans CJK as fallback for Japanese,
/// Chinese and Korean names. Glyphs are rasterized once per size.
pub struct Fonts {
    fonts: Vec<fontdue::Font>,
    cache: HashMap<(usize, char, u32), (fontdue::Metrics, Vec<u8>)>,
}

const FONT_CANDIDATES: &[&[&str]] = &[
    &[
        "/usr/share/fonts/noto/NotoSans-Medium.ttf",
        "/usr/share/fonts/google-noto/NotoSans-Medium.ttf",
        "/usr/share/fonts/truetype/noto/NotoSans-Medium.ttf",
        "/usr/share/fonts/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/google-noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf",
    ],
    &[
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Medium.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/google-noto-sans-cjk-fonts/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    ],
];

impl Fonts {
    pub fn load() -> anyhow::Result<Self> {
        let mut fonts = Vec::new();
        for candidates in FONT_CANDIDATES {
            for path in *candidates {
                let Ok(bytes) = std::fs::read(path) else {
                    continue;
                };
                // Collection index 0 of Noto Sans CJK is the Japanese variant.
                let settings = fontdue::FontSettings {
                    collection_index: 0,
                    scale: 48.0,
                    ..Default::default()
                };
                if let Ok(font) = fontdue::Font::from_bytes(bytes, settings) {
                    fonts.push(font);
                    break;
                }
            }
        }
        anyhow::ensure!(
            !fonts.is_empty(),
            "No usable font found (Noto Sans or DejaVu Sans)"
        );
        Ok(Self {
            fonts,
            cache: HashMap::new(),
        })
    }

    fn font_for(&self, ch: char) -> usize {
        self.fonts
            .iter()
            .position(|f| f.lookup_glyph_index(ch) != 0)
            .unwrap_or(0)
    }

    fn glyph(&mut self, ch: char, size: f32) -> &(fontdue::Metrics, Vec<u8>) {
        let font = self.font_for(ch);
        let key = (font, ch, size.to_bits());
        self.cache
            .entry(key)
            .or_insert_with(|| self.fonts[font].rasterize(ch, size))
    }

    pub fn measure(&mut self, text: &str, size: f32) -> f32 {
        text.chars()
            .map(|c| self.glyph(c, size).0.advance_width)
            .sum()
    }

    /// Draws `text` with its baseline at `y`; truncates with "…" past `max_width`.
    /// Returns the drawn width.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        canvas: &mut Canvas,
        text: &str,
        x: f32,
        y: f32,
        size: f32,
        color: Rgb,
        max_width: f32,
    ) -> f32 {
        let mut shown: String = text.to_string();
        if self.measure(text, size) > max_width {
            let ellipsis = self.measure("…", size);
            let mut width = 0.0;
            shown.clear();
            for c in text.chars() {
                let advance = self.glyph(c, size).0.advance_width;
                if width + advance + ellipsis > max_width {
                    break;
                }
                width += advance;
                shown.push(c);
            }
            shown.push('…');
        }
        let mut pen = x;
        for c in shown.chars() {
            let (metrics, coverage) = self.glyph(c, size).clone();
            let gx = pen.round() as i32 + metrics.xmin;
            let gy = (y.round() as i32) - metrics.height as i32 - metrics.ymin;
            for row in 0..metrics.height {
                for col in 0..metrics.width {
                    let a = coverage[row * metrics.width + col] as f32 / 255.0;
                    canvas.blend(gx + col as i32, gy + row as i32, color, a);
                }
            }
            pen += metrics.advance_width;
        }
        pen - x
    }

    /// Word-wraps `text` into lines no wider than `max_width`.
    pub fn wrap(&mut self, text: &str, size: f32, max_width: f32) -> Vec<String> {
        let mut lines = Vec::new();
        let mut line = String::new();
        for word in text.split_whitespace() {
            let candidate = if line.is_empty() {
                word.to_string()
            } else {
                format!("{line} {word}")
            };
            if self.measure(&candidate, size) > max_width && !line.is_empty() {
                lines.push(std::mem::take(&mut line));
                line = word.to_string();
            } else {
                line = candidate;
            }
        }
        if !line.is_empty() {
            lines.push(line);
        }
        lines
    }
}

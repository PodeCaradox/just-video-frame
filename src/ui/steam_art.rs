//! Steam library artwork (capsules, hero, icon) for the non-Steam shortcut.
//! Rendered with the UI canvas so it can be regenerated without image tools:
//! `just-video steam-art assets/steam`.

use super::canvas::{Canvas, Fonts, Rgb};

// The browser's palette (src/ui/browser.rs).
const ACCENT: Rgb = [0x4f, 0x8c, 0xff];
const TEXT: Rgb = [0xe8, 0xea, 0xed];
const SUBTLE: Rgb = [0x9a, 0xa0, 0xa6];
const TOP: Rgb = [0x1f, 0x26, 0x34];
const BOTTOM: Rgb = [0x0c, 0x0e, 0x12];

/// File name and size of each piece of art. install-frame.sh maps these to
/// Steam's grid names (`<appid>p.png`, `<appid>.png`, `<appid>_hero.png`).
pub const PIECES: &[(&str, u32, u32)] = &[
    ("portrait.png", 600, 900),
    ("capsule.png", 920, 430),
    ("hero.png", 1920, 620),
    ("icon.png", 256, 256),
];

pub fn render(name: &str, width: u32, height: u32, fonts: &mut Fonts) -> Canvas {
    let mut c = Canvas::new(width, height);
    let (w, h) = (width as f32, height as f32);
    match name {
        "portrait.png" => {
            background(&mut c, (w / 2.0, h * 0.4), h * 0.45);
            perforations(&mut c, 34.0);
            emblem(&mut c, w / 2.0, h * 0.4, 150.0);
            centered(&mut c, fonts, "Just Video", h * 0.72, 92.0, TEXT);
            centered(
                &mut c,
                fonts,
                "VR video player",
                h * 0.72 + 64.0,
                36.0,
                SUBTLE,
            );
        }
        "capsule.png" => {
            background(&mut c, (w * 0.25, h / 2.0), h * 0.7);
            emblem(&mut c, w * 0.25, h / 2.0, 120.0);
            let x = w * 0.46;
            fonts.draw(&mut c, "Just Video", x, h / 2.0 + 20.0, 88.0, TEXT, w);
            fonts.draw(
                &mut c,
                "VR video player",
                x + 4.0,
                h / 2.0 + 80.0,
                34.0,
                SUBTLE,
                w,
            );
        }
        // Steam draws the title over the hero, so it carries no text.
        "hero.png" => {
            background(&mut c, (w * 0.72, h / 2.0), h * 0.8);
            perforations(&mut c, 30.0);
            emblem(&mut c, w * 0.72, h / 2.0, 170.0);
        }
        _ => {
            background(&mut c, (w / 2.0, h / 2.0), w * 0.6);
            emblem(&mut c, w / 2.0, h / 2.0, w * 0.36);
        }
    }
    c
}

/// Dark vertical gradient with a soft accent glow behind the emblem.
fn background(c: &mut Canvas, glow: (f32, f32), radius: f32) {
    let h = c.height as f32;
    for y in 0..c.height {
        let t = y as f32 / (h - 1.0);
        for x in 0..c.width {
            let d = ((x as f32 - glow.0).powi(2) + (y as f32 - glow.1).powi(2)).sqrt() / radius;
            let g = (1.0 - d).clamp(0.0, 1.0).powi(2) * 0.28;
            let i = ((y * c.width + x) * 4) as usize;
            for k in 0..3 {
                let base = TOP[k] as f32 + (BOTTOM[k] as f32 - TOP[k] as f32) * t;
                c.pixels[i + k] = (base + (ACCENT[k] as f32 - base) * g).round() as u8;
            }
            c.pixels[i + 3] = 255;
        }
    }
}

/// Film-strip holes along the top and bottom edges.
fn perforations(c: &mut Canvas, size: f32) {
    let (w, h) = (c.width as f32, c.height as f32);
    let hole = [0x05, 0x06, 0x08];
    let step = size * 1.8;
    let count = (w / step).floor();
    let x0 = (w - count * step) / 2.0 + (step - size) / 2.0;
    for i in 0..count as usize {
        let x = x0 + i as f32 * step;
        c.rect(x, size * 0.5, size, size * 0.7, size * 0.15, hole);
        c.rect(x, h - size * 1.2, size, size * 0.7, size * 0.15, hole);
    }
}

/// Accent disc with a play triangle.
fn emblem(c: &mut Canvas, cx: f32, cy: f32, r: f32) {
    c.circle(cx, cy + r * 0.04, r * 1.02, [0x08, 0x0a, 0x0e]);
    c.circle(cx, cy, r, ACCENT);
    // Shifted right so the triangle's centroid sits on the disc's centre.
    let s = r * 0.48;
    let x = cx + s * 0.25;
    triangle(
        c,
        [(x - s * 0.75, cy - s), (x - s * 0.75, cy + s), (x + s, cy)],
        TEXT,
    );
}

/// Filled triangle, anti-aliased by 4×4 supersampling.
fn triangle(c: &mut Canvas, p: [(f32, f32); 3], color: Rgb) {
    let edge = |a: (f32, f32), b: (f32, f32), x: f32, y: f32| {
        (b.0 - a.0) * (y - a.1) - (b.1 - a.1) * (x - a.0)
    };
    let min_x = p.iter().map(|q| q.0).fold(f32::MAX, f32::min).floor() as i32;
    let max_x = p.iter().map(|q| q.0).fold(f32::MIN, f32::max).ceil() as i32;
    let min_y = p.iter().map(|q| q.1).fold(f32::MAX, f32::min).floor() as i32;
    let max_y = p.iter().map(|q| q.1).fold(f32::MIN, f32::max).ceil() as i32;
    let sign = edge(p[0], p[1], p[2].0, p[2].1).signum();
    for py in min_y.max(0)..max_y.min(c.height as i32) {
        for px in min_x.max(0)..max_x.min(c.width as i32) {
            let mut hits = 0;
            for sy in 0..4 {
                for sx in 0..4 {
                    let (x, y) = (
                        px as f32 + (sx as f32 + 0.5) / 4.0,
                        py as f32 + (sy as f32 + 0.5) / 4.0,
                    );
                    if (0..3).all(|k| edge(p[k], p[(k + 1) % 3], x, y) * sign >= 0.0) {
                        hits += 1;
                    }
                }
            }
            let a = hits as f32 / 16.0;
            let i = ((py as u32 * c.width + px as u32) * 4) as usize;
            for (dst, src) in c.pixels[i..i + 3].iter_mut().zip(color) {
                *dst = (*dst as f32 + (src as f32 - *dst as f32) * a).round() as u8;
            }
        }
    }
}

fn centered(c: &mut Canvas, fonts: &mut Fonts, text: &str, y: f32, size: f32, color: Rgb) {
    let x = (c.width as f32 - fonts.measure(text, size)) / 2.0;
    fonts.draw(c, text, x, y, size, color, f32::MAX);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_every_piece_at_its_size() {
        let mut fonts = Fonts::load().expect("fonts");
        for &(name, w, h) in PIECES {
            let c = render(name, w, h, &mut fonts);
            assert_eq!((c.width, c.height), (w, h));
            // Opaque everywhere: Steam shows transparent capsule pixels as black.
            assert!(c.pixels.chunks_exact(4).all(|p| p[3] == 255), "{name}");
        }
    }
}

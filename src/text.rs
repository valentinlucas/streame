//! Rendu de texte (police DejaVu embarquée) en images RGBA, pour les libellés à l'écran
//! et les touches du Stream Deck.

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use image::{Rgba, RgbaImage};
use std::sync::OnceLock;

const FONT_BYTES: &[u8] = include_bytes!("../assets/fonts/DejaVuSans-Bold.ttf");

pub fn font() -> &'static FontRef<'static> {
    static FONT: OnceLock<FontRef<'static>> = OnceLock::new();
    FONT.get_or_init(|| FontRef::try_from_slice(FONT_BYTES).expect("police embarquée invalide"))
}

/// Taille en pixels à partir d'une description Pango (« Sans Bold 48 » → 48 × 4/3).
pub fn px_from_desc(desc: &str, default: f32) -> f32 {
    desc.split_whitespace()
        .last()
        .and_then(|s| s.parse::<f32>().ok())
        .map(|pt| pt * 4.0 / 3.0)
        .unwrap_or(default)
}

pub fn text_width(text: &str, px: f32) -> f32 {
    let scaled = font().as_scaled(PxScale::from(px));
    let mut w = 0.0;
    let mut prev = None;
    for ch in text.chars() {
        let id = scaled.glyph_id(ch);
        if let Some(p) = prev {
            w += scaled.kern(p, id);
        }
        w += scaled.h_advance(id);
        prev = Some(id);
    }
    w
}

/// Dessine une ligne de texte dans `img` (origine = coin haut-gauche de la ligne).
pub fn draw_line(img: &mut RgbaImage, text: &str, px: f32, x: f32, y: f32, color: [u8; 4]) {
    let f = font();
    let scaled = f.as_scaled(PxScale::from(px));
    let (w, h) = img.dimensions();
    let mut cx = x;
    let baseline = y + scaled.ascent();
    let mut prev = None;
    for ch in text.chars() {
        let id = f.glyph_id(ch);
        if let Some(p) = prev {
            cx += scaled.kern(p, id);
        }
        let glyph = id.with_scale_and_position(PxScale::from(px), ab_glyph::point(cx, baseline));
        if let Some(outlined) = f.outline_glyph(glyph) {
            let b = outlined.px_bounds();
            outlined.draw(|gx, gy, c| {
                let ix = b.min.x as i32 + gx as i32;
                let iy = b.min.y as i32 + gy as i32;
                if ix >= 0 && iy >= 0 && (ix as u32) < w && (iy as u32) < h && c > 0.002 {
                    let p = img.get_pixel_mut(ix as u32, iy as u32);
                    let a = c * color[3] as f32 / 255.0;
                    for k in 0..3 {
                        p.0[k] = (p.0[k] as f32 * (1.0 - a) + color[k] as f32 * a) as u8;
                    }
                    p.0[3] = (p.0[3] as f32 * (1.0 - a) + 255.0 * a) as u8;
                }
            });
        }
        cx += scaled.h_advance(id);
        prev = Some(id);
    }
}

/// Image RGBA ajustée au texte, avec un fond optionnel et une marge.
pub fn render_label(
    text: &str,
    px: f32,
    color: [u8; 4],
    background: Option<[u8; 4]>,
    padding: u32,
) -> RgbaImage {
    let scaled = font().as_scaled(PxScale::from(px));
    let w = (text_width(text, px).ceil() as u32 + 2 * padding).max(1);
    let h = (scaled.height().ceil() as u32 + 2 * padding).max(1);
    let mut img = RgbaImage::from_pixel(w, h, Rgba(background.unwrap_or([0, 0, 0, 0])));
    draw_line(&mut img, text, px, padding as f32, padding as f32, color);
    img
}

/// Texte multi-ligne centré dans une boîte `w`×`h`, taille réduite jusqu'à tenir.
pub fn render_centered(
    lines: &[String],
    w: u32,
    h: u32,
    mut px: f32,
    color: [u8; 4],
    background: [u8; 4],
) -> RgbaImage {
    let margin = w as f32 * 0.92;
    loop {
        let widest = lines.iter().map(|l| text_width(l, px)).fold(0.0, f32::max);
        if widest <= margin || px <= 8.0 {
            break;
        }
        px -= 1.0;
    }
    let scaled = font().as_scaled(PxScale::from(px));
    let line_h = scaled.height() + scaled.line_gap();
    let total = line_h * lines.len() as f32;
    let mut img = RgbaImage::from_pixel(w, h, Rgba(background));
    let mut y = (h as f32 - total) / 2.0;
    for line in lines {
        let x = (w as f32 - text_width(line, px)) / 2.0;
        draw_line(&mut img, line, px, x, y, color);
        y += line_h;
    }
    img
}

/// Coupe un libellé en deux lignes équilibrées s'il est long.
pub fn wrap_label(label: &str) -> Vec<String> {
    let words: Vec<&str> = label.split_whitespace().collect();
    if words.len() <= 1 || label.len() <= 8 {
        return vec![label.to_string()];
    }
    let mut best = (usize::MAX, 0);
    for split in 1..words.len() {
        let a = words[..split].join(" ");
        let b = words[split..].join(" ");
        let diff = a.len().abs_diff(b.len());
        if diff < best.0 {
            best = (diff, split);
        }
    }
    vec![words[..best.1].join(" "), words[best.1..].join(" ")]
}

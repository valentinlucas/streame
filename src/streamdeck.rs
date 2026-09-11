//! Contrôle des scènes par Stream Deck (Elgato).

use crate::config::{parse_color, ButtonConfig, Config};
use crate::engine::Engine;
use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use elgato_streamdeck::{list_devices, new_hidapi, StreamDeck, StreamDeckInput};
use image::{DynamicImage, Rgb, RgbImage};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tracing::{error, info, warn};

const FONT_BYTES: &[u8] = include_bytes!("../assets/fonts/DejaVuSans-Bold.ttf");

#[derive(Debug, Clone)]
enum Action {
    Program(usize),
    Cut(usize),
    Preview(usize),
    Take,
}

#[derive(Debug, Clone)]
struct Button {
    index: u8,
    label: String,
    color: (u8, u8, u8),
    action: Action,
}

fn build_buttons(cfg: &Config, engine: &Engine) -> Vec<Button> {
    let scenes = engine.scenes();
    let mut buttons = Vec::new();
    let configured: Vec<ButtonConfig> = if cfg.streamdeck.buttons.is_empty() {
        let mut v: Vec<ButtonConfig> = scenes
            .iter()
            .enumerate()
            .map(|(i, s)| ButtonConfig {
                index: i as u8,
                scene: s.id.clone(),
                ..Default::default()
            })
            .collect();
        v.push(ButtonConfig {
            index: scenes.len() as u8,
            action: "take".into(),
            label: "TAKE".into(),
            ..Default::default()
        });
        v
    } else {
        cfg.streamdeck.buttons.clone()
    };
    for b in configured {
        let scene_idx = engine.scene_index(&b.scene);
        let action = match b.action.to_lowercase().as_str() {
            "take" => Action::Take,
            "cut" | "preview" | "program" | "" => match scene_idx {
                Some(i) => match b.action.to_lowercase().as_str() {
                    "cut" => Action::Cut(i),
                    "preview" => Action::Preview(i),
                    _ => Action::Program(i),
                },
                None => {
                    warn!(
                        "Stream Deck : touche {} → scène « {} » inconnue",
                        b.index, b.scene
                    );
                    continue;
                }
            },
            other => {
                warn!("Stream Deck : action inconnue « {other} »");
                continue;
            }
        };
        let label = if !b.label.is_empty() {
            b.label.clone()
        } else {
            scene_idx
                .map(|i| scenes[i].name.clone())
                .unwrap_or_else(|| b.action.to_uppercase())
        };
        let color = parse_color(&b.color)
            .map(|(r, g, b, _)| (r, g, b))
            .unwrap_or(match action {
                Action::Take => (40, 40, 120),
                _ => (40, 40, 40),
            });
        buttons.push(Button {
            index: b.index,
            label,
            color,
            action,
        });
    }
    buttons
}

/// Lance le thread Stream Deck (reconnexion automatique).
pub fn spawn(cfg: Arc<Config>, engine: Arc<Engine>) -> Option<JoinHandle<()>> {
    if !cfg.streamdeck.enabled {
        return None;
    }
    let font = match FontRef::try_from_slice(FONT_BYTES) {
        Ok(f) => f,
        Err(e) => {
            error!("Stream Deck : police illisible : {e}");
            return None;
        }
    };
    let handle = thread::Builder::new()
        .name("streamdeck".into())
        .spawn(move || {
            let buttons = build_buttons(&cfg, &engine);
            let mut warned = false;
            loop {
                match connect(&cfg) {
                    Some(deck) => {
                        warned = false;
                        run_deck(&deck, &cfg, &engine, &buttons, &font);
                        warn!("Stream Deck déconnecté, nouvelle tentative dans 3 s");
                    }
                    None => {
                        if !warned {
                            info!("aucun Stream Deck détecté (nouvelle tentative toutes les 3 s)");
                            warned = true;
                        }
                    }
                }
                thread::sleep(Duration::from_secs(3));
            }
        })
        .ok()?;
    Some(handle)
}

fn connect(cfg: &Config) -> Option<StreamDeck> {
    let hid = new_hidapi().ok()?;
    let devices = list_devices(&hid);
    let (kind, serial) = devices
        .iter()
        .find(|(_, s)| cfg.streamdeck.serial.is_empty() || *s == cfg.streamdeck.serial)
        .cloned()?;
    match StreamDeck::connect(&hid, kind, &serial) {
        Ok(d) => {
            info!(
                "Stream Deck connecté : {:?} ({serial}), {} touches",
                kind,
                kind.key_count()
            );
            Some(d)
        }
        Err(e) => {
            warn!("Stream Deck : connexion impossible : {e}");
            None
        }
    }
}

fn run_deck(
    deck: &StreamDeck,
    cfg: &Config,
    engine: &Arc<Engine>,
    buttons: &[Button],
    font: &FontRef<'static>,
) {
    let key_count = deck.kind().key_count() as usize;
    let _ = deck.reset();
    let _ = deck.set_brightness(cfg.streamdeck.brightness.min(100));
    let mut prev_state = vec![false; key_count];
    let mut last = (usize::MAX, usize::MAX);
    loop {
        let cur = (engine.program_index(), engine.preview_index());
        if cur != last {
            last = cur;
            if let Err(e) = render_all(deck, buttons, cur, font) {
                warn!("Stream Deck : rendu : {e}");
                return;
            }
        }
        match deck.read_input(Some(Duration::from_millis(50))) {
            Ok(StreamDeckInput::ButtonStateChange(state)) => {
                for (i, pressed) in state.iter().enumerate() {
                    if *pressed && !prev_state.get(i).copied().unwrap_or(false) {
                        if let Some(b) = buttons.iter().find(|b| b.index as usize == i) {
                            match b.action {
                                Action::Program(s) => engine.transition_to(s),
                                Action::Cut(s) => engine.cut(s),
                                Action::Preview(s) => engine.set_preview(s),
                                Action::Take => engine.take(),
                            }
                        }
                    }
                }
                prev_state = state;
            }
            Ok(_) => {}
            Err(e) => {
                warn!("Stream Deck : lecture : {e}");
                return;
            }
        }
    }
}

fn render_all(
    deck: &StreamDeck,
    buttons: &[Button],
    (program, preview): (usize, usize),
    font: &FontRef<'static>,
) -> Result<(), elgato_streamdeck::StreamDeckError> {
    let format = deck.kind().key_image_format();
    let (w, h) = format.size;
    if w == 0 || h == 0 {
        return Ok(());
    }
    let key_count = deck.kind().key_count();
    for key in 0..key_count {
        let img = match buttons.iter().find(|b| b.index == key) {
            Some(b) => {
                let (bg, fg) = match b.action {
                    Action::Program(s) | Action::Cut(s) if s == program => {
                        ((200, 30, 30), (255, 255, 255))
                    }
                    Action::Program(s) | Action::Cut(s) | Action::Preview(s) if s == preview => {
                        ((30, 160, 60), (255, 255, 255))
                    }
                    _ => (b.color, (230, 230, 230)),
                };
                render_button(w as u32, h as u32, &b.label, bg, fg, font)
            }
            None => RgbImage::from_pixel(w as u32, h as u32, Rgb([0, 0, 0])),
        };
        deck.set_button_image(key, DynamicImage::ImageRgb8(img))?;
    }
    deck.flush()
}

/// Dessine un bouton : fond coloré + texte centré (2 lignes max).
fn render_button(
    w: u32,
    h: u32,
    label: &str,
    bg: (u8, u8, u8),
    fg: (u8, u8, u8),
    font: &FontRef<'static>,
) -> RgbImage {
    let mut img = RgbImage::from_pixel(w, h, Rgb([bg.0, bg.1, bg.2]));
    let lines = wrap_label(label);
    let mut px = (h as f32 / 3.2).max(10.0);
    let margin = w as f32 * 0.9;
    // Réduit la taille jusqu'à ce que toutes les lignes tiennent.
    loop {
        let scaled = font.as_scaled(PxScale::from(px));
        let widest = lines
            .iter()
            .map(|l| text_width(&scaled, l))
            .fold(0.0, f32::max);
        if widest <= margin || px <= 8.0 {
            break;
        }
        px -= 1.0;
    }
    let scaled = font.as_scaled(PxScale::from(px));
    let line_h = scaled.height() + scaled.line_gap();
    let total_h = line_h * lines.len() as f32;
    let mut y = (h as f32 - total_h) / 2.0 + scaled.ascent();
    for line in &lines {
        let tw = text_width(&scaled, line);
        let mut x = (w as f32 - tw) / 2.0;
        let mut prev: Option<ab_glyph::GlyphId> = None;
        for ch in line.chars() {
            let id = font.glyph_id(ch);
            if let Some(p) = prev {
                x += scaled.kern(p, id);
            }
            let glyph = id.with_scale_and_position(PxScale::from(px), ab_glyph::point(x, y));
            if let Some(outlined) = font.outline_glyph(glyph) {
                let bounds = outlined.px_bounds();
                outlined.draw(|gx, gy, c| {
                    let ix = bounds.min.x as i32 + gx as i32;
                    let iy = bounds.min.y as i32 + gy as i32;
                    if ix >= 0 && iy >= 0 && (ix as u32) < w && (iy as u32) < h {
                        let p = img.get_pixel_mut(ix as u32, iy as u32);
                        for k in 0..3 {
                            let f = [fg.0, fg.1, fg.2][k] as f32;
                            p.0[k] = (p.0[k] as f32 * (1.0 - c) + f * c) as u8;
                        }
                    }
                });
            }
            x += scaled.h_advance(id);
            prev = Some(id);
        }
        y += line_h;
    }
    img
}

fn text_width<F: Font>(scaled: &ab_glyph::PxScaleFont<F>, text: &str) -> f32 {
    text.chars()
        .map(|c| scaled.h_advance(scaled.glyph_id(c)))
        .sum()
}

fn wrap_label(label: &str) -> Vec<String> {
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

//! Contrôle des scènes par Stream Deck (Elgato).

use crate::config::{parse_color, ButtonConfig, Config};
use crate::engine::Engine;
use crate::text;
use elgato_streamdeck::{list_devices, new_hidapi, StreamDeck, StreamDeckInput};
use image::{DynamicImage, Rgba, RgbaImage};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tracing::{info, warn};

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
    let handle = thread::Builder::new()
        .name("streamdeck".into())
        .spawn(move || {
            let buttons = build_buttons(&cfg, &engine);
            let mut warned = false;
            loop {
                match connect(&cfg) {
                    Some(deck) => {
                        warned = false;
                        run_deck(&deck, &cfg, &engine, &buttons);
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

fn run_deck(deck: &StreamDeck, cfg: &Config, engine: &Arc<Engine>, buttons: &[Button]) {
    let key_count = deck.kind().key_count() as usize;
    let _ = deck.reset();
    let _ = deck.set_brightness(cfg.streamdeck.brightness.min(100));
    let mut prev_state = vec![false; key_count];
    let mut last = (usize::MAX, usize::MAX);
    loop {
        let cur = (engine.program_index(), engine.preview_index());
        if cur != last {
            last = cur;
            if let Err(e) = render_all(deck, buttons, cur) {
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
                let lines = text::wrap_label(&b.label);
                text::render_centered(
                    &lines,
                    w as u32,
                    h as u32,
                    (h as f32 / 3.2).max(10.0),
                    [fg.0, fg.1, fg.2, 255],
                    [bg.0, bg.1, bg.2, 255],
                )
            }
            None => RgbaImage::from_pixel(w as u32, h as u32, Rgba([0, 0, 0, 255])),
        };
        deck.set_button_image(key, DynamicImage::ImageRgba8(img))?;
    }
    deck.flush()
}

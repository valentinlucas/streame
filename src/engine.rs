//! Moteur : état de la régie (scènes, programme, preview, transitions), sources d'images et
//! audio de la carte. Toute la chaîne média est native (aucun GStreamer) :
//!
//! ```text
//! webrtc-rs ─ VideoToolbox (vt.rs) ─► IOSurface ─► FrameSlot(phone) ─┐
//! AVFoundation (avf.rs, overlay.mp4) ─► IOSurface ─► FrameSlot(vidéo) ┼─► rendu wgpu/Metal ─► HDMI + multiview
//! PNG / texte (décodés au démarrage) ─────────────────────────────────┘
//! webrtc-rs ─ libopus ─► anneau stream ──────────────────────────────┐
//! AVFoundation (son des vidéos) ─► bus d'habillage (somme) ─► anneau ┼─► callback CoreAudio (cpal) :
//!                                                                    │   mixage + routage + VU-mètres
//!                                                                    └─► carte (Wing), N canaux
//! CoreAudio in (Wing) ─ callback (sélection canaux + VU) ─► anneau ─► libopus ─► webrtc-rs ─► téléphone
//! ```
//! Le GPU ne reçoit que des IOSurfaces (zéro copie) ; le CPU ne fait que du contrôle, de l'Opus
//! et le mixage audio à l'horloge de la carte.

use crate::audio;
use crate::avf;
use crate::config::{parse_color, Config, Geometry, LayerConfig};
use crate::frame::{FpsCounter, FrameSlot};
use crate::text;
use anyhow::{Context, Result};
use image::RgbaImage;
use serde::Serialize;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tracing::{error, info, warn};

pub const NO_PHONE_TEXT: &str = "Connexion avec notre correspondant perdue !";

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Program {
        scene: String,
    },
    Preview {
        scene: String,
    },
    Phone {
        connected: bool,
        name: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct SceneInfo {
    pub id: String,
    pub name: String,
}

/// Statistiques RTP côté Mac (webrtc-rs `get_stats`, flux vidéo entrant).
#[derive(Debug, Clone, Serialize, Default)]
pub struct RtpStats {
    pub codec: String,
    pub bitrate_kbps: f32,
    pub packets_received: u64,
    pub packets_lost: i64,
    pub loss_percent: f32,
    pub jitter_ms: f32,
    pub nack_count: u32,
    pub pli_count: u32,
    pub rtt_ms: Option<f32>,
}

/// Statistiques envoyées par la page du téléphone (encodeur et réseau vus de son côté).
#[derive(Debug, Clone, Serialize, serde::Deserialize, Default)]
#[serde(default)]
pub struct PhoneStats {
    pub width: u32,
    pub height: u32,
    pub fps: f32,
    pub bitrate_kbps: f32,
    pub quality_limitation: String,
    pub rtt_ms: Option<f32>,
    pub codec: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Stats {
    pub phone_fps: f32,
    pub phone_width: u32,
    pub phone_height: u32,
    pub render_fps: f32,
    pub rtp: Option<RtpStats>,
    pub phone: Option<PhoneStats>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub scenes: Vec<SceneInfo>,
    pub program: String,
    pub preview: String,
    pub phone_connected: bool,
    pub phone_name: Option<String>,
    pub transition: String,
    pub stats: Stats,
}

/// Niveau audio d'une source (VU-mètre), en dBFS par canal.
#[derive(Debug, Clone, Serialize, Default)]
pub struct AudioMeter {
    pub id: String,
    pub label: String,
    pub rms_db: Vec<f32>,
    pub peak_db: Vec<f32>,
}

/// Affectation des canaux (1-based) pour chaque source audio.
#[derive(Debug, Clone, Serialize, Default)]
pub struct AudioRoute {
    /// Canaux de sortie recevant le stream WebRTC.
    pub stream: Vec<usize>,
    /// Canaux de sortie recevant l'habillage.
    pub branding: Vec<usize>,
    /// Canaux d'entrée renvoyés au téléphone.
    pub return_input: Vec<usize>,
    pub out_channels: i32,
    pub in_channels: i32,
}

/// Poignées de l'audio cpal : sélections de canaux modifiables à chaud (lues dans les callbacks
/// temps réel), injection du son du téléphone, VU-mètres, lecture du retour.
#[derive(Default, Clone)]
struct AudioCtl {
    /// Sélection des 2 canaux d'entrée renvoyés au téléphone (indices 0-based).
    return_sel: Option<Arc<[AtomicUsize; 2]>>,
    /// Canaux de sortie (0-based) du stream et de l'habillage.
    stream_ch: Option<Arc<[AtomicUsize; 2]>>,
    branding_ch: Option<Arc<[AtomicUsize; 2]>>,
    /// Injection du son du téléphone dans le mixeur cpal.
    phone_pusher: Option<audio::cpal_out::Pusher>,
    /// VU-mètres calculés dans les callbacks cpal.
    meters_cpal: Option<Arc<audio::cpal_out::Meters>>,
    /// Lecture du retour (entrée carte) pour l'encodage Opus côté webrtc-rs.
    return_reader: Option<audio::cpal_out::Reader>,
    out_channels: i32,
    in_channels: i32,
}

/// Résultat de la mise en place de l'audio.
#[derive(Default)]
struct AudioSetup {
    ctl: AudioCtl,
    meters: Vec<(String, String)>,
    route: AudioRoute,
    /// Flux CoreAudio à garder en vie.
    keepalives: Vec<Box<dyn std::any::Any + Send>>,
    /// Anneau d'habillage de la sortie (alimenté par le bus d'habillage).
    branding: Option<audio::cpal_out::Pusher>,
}

/// Contenu d'un calque, prêt pour le GPU.
pub enum LayerKind {
    Phone,
    Color([f32; 4]),
    Image(Arc<RgbaImage>),
    Video(Arc<FrameSlot>),
    Text(Arc<RgbaImage>),
}

pub struct Layer {
    /// Clé unique pour le cache de textures.
    pub id: u64,
    pub kind: LayerKind,
    pub geometry: Geometry,
    pub opacity: f32,
}

pub struct Scene {
    pub id: String,
    pub name: String,
    pub layers: Vec<Layer>,
}

#[derive(Debug, Clone, Copy)]
pub struct Transition {
    pub from: usize,
    pub start: Instant,
    pub duration: Duration,
}

impl Transition {
    /// Progression 0..1.
    pub fn progress(&self) -> f32 {
        if self.duration.is_zero() {
            return 1.0;
        }
        (self.start.elapsed().as_secs_f32() / self.duration.as_secs_f32()).clamp(0.0, 1.0)
    }
}

struct State {
    program: usize,
    preview: usize,
    transition: Option<Transition>,
    phone: Option<String>,
}

/// Ce que le rendu doit savoir pour dessiner une image.
#[derive(Debug, Clone, Copy)]
pub struct RenderState {
    pub program: usize,
    pub preview: usize,
    /// (scène de départ, progression) pendant un fondu.
    pub fade: Option<(usize, f32)>,
    pub phone_connected: bool,
}

pub struct Engine {
    cfg: Arc<Config>,
    scenes: Vec<Scene>,
    phone: Arc<FrameSlot>,
    state: Mutex<State>,
    events: broadcast::Sender<Event>,
    pub render_fps: FpsCounter,
    rtp_stats: Mutex<Option<RtpStats>>,
    phone_stats: Mutex<Option<PhoneStats>>,
    meter_order: Vec<(String, String)>,
    audio_ctl: AudioCtl,
    route: Mutex<AudioRoute>,
    /// Objets maintenus en vie tant que le moteur tourne : flux CoreAudio, lecteurs de fichiers
    /// vidéo, bus d'habillage. Vidés par `stop()` (arrêt propre des threads et de la carte).
    keepalive: Mutex<Vec<Box<dyn std::any::Any + Send>>>,
}

fn color_to_f32(c: (u8, u8, u8, u8)) -> [f32; 4] {
    [
        c.0 as f32 / 255.0,
        c.1 as f32 / 255.0,
        c.2 as f32 / 255.0,
        c.3 as f32 / 255.0,
    ]
}

impl Engine {
    pub fn new(cfg: Arc<Config>) -> Result<Arc<Engine>> {
        let (events, _) = broadcast::channel(64);
        let mut next_layer_id = 1u64;

        // Audio d'abord : le bus d'habillage doit exister avant les vidéos, dont le son s'y branche.
        let mut setup = if cfg.audio.enabled {
            match Self::build_audio(&cfg) {
                Ok(x) => x,
                Err(e) => {
                    error!("audio désactivé : {e:#}");
                    AudioSetup::default()
                }
            }
        } else {
            AudioSetup::default()
        };
        let bus = setup
            .branding
            .take()
            .map(|p| audio::BrandingBus::start(p, cfg.audio.sample_rate as u32));
        let mut keepalive = std::mem::take(&mut setup.keepalives);

        let mut scenes = Vec::new();
        for sc in &cfg.scenes {
            let mut layers = Vec::new();
            let mut cfg_layers = sc.layers.clone();
            if cfg_layers.is_empty() {
                cfg_layers.push(LayerConfig::Color {
                    color: "#000000".into(),
                    geometry: Geometry::default(),
                });
            }
            for (li, lc) in cfg_layers.iter().enumerate() {
                match Self::build_layer(&cfg, lc, next_layer_id, bus.as_ref(), &mut keepalive) {
                    Ok(layer) => layers.push(layer),
                    Err(e) => warn!("scène « {} », calque {} ignoré : {e:#}", sc.name, li + 1),
                }
                next_layer_id += 1;
            }
            scenes.push(Scene {
                id: sc.id.clone(),
                name: sc.name.clone(),
                layers,
            });
        }
        if let Some(bus) = bus {
            keepalive.push(Box::new(bus));
        }

        Ok(Arc::new(Engine {
            cfg: cfg.clone(),
            scenes,
            phone: Arc::new(FrameSlot::new()),
            state: Mutex::new(State {
                program: 0,
                preview: 0,
                transition: None,
                phone: None,
            }),
            events,
            render_fps: FpsCounter::new(),
            rtp_stats: Mutex::new(None),
            phone_stats: Mutex::new(None),
            meter_order: setup.meters,
            audio_ctl: setup.ctl,
            route: Mutex::new(setup.route),
            keepalive: Mutex::new(keepalive),
        }))
    }

    fn build_layer(
        cfg: &Config,
        lc: &LayerConfig,
        id: u64,
        bus: Option<&audio::BrandingBus>,
        keepalive: &mut Vec<Box<dyn std::any::Any + Send>>,
    ) -> Result<Layer> {
        Ok(match lc {
            LayerConfig::Phone { geometry, opacity } => Layer {
                id,
                kind: LayerKind::Phone,
                geometry: geometry.clone(),
                opacity: *opacity as f32,
            },
            LayerConfig::Color { color, geometry } => {
                let c =
                    parse_color(color).with_context(|| format!("couleur invalide : {color}"))?;
                Layer {
                    id,
                    kind: LayerKind::Color(color_to_f32(c)),
                    geometry: geometry.clone(),
                    opacity: 1.0,
                }
            }
            LayerConfig::Text {
                text: t,
                font,
                color,
                geometry,
            } => {
                let c =
                    parse_color(color).with_context(|| format!("couleur invalide : {color}"))?;
                let px = text::px_from_desc(font, 64.0);
                let img = text::render_label(t, px, [c.0, c.1, c.2, c.3], None, 4);
                Layer {
                    id,
                    kind: LayerKind::Text(Arc::new(img)),
                    geometry: geometry.clone(),
                    opacity: 1.0,
                }
            }
            LayerConfig::Image {
                path,
                geometry,
                opacity,
            } => {
                let file = cfg.resolve(path);
                let img = image::open(&file)
                    .with_context(|| format!("image illisible : {}", file.display()))?
                    .to_rgba8();
                Layer {
                    id,
                    kind: LayerKind::Image(Arc::new(img)),
                    geometry: geometry.clone(),
                    opacity: *opacity as f32,
                }
            }
            LayerConfig::Video {
                path,
                looped,
                geometry,
                opacity,
            } => {
                let file = cfg.resolve(path);
                anyhow::ensure!(file.is_file(), "vidéo introuvable : {}", file.display());
                let slot = Arc::new(FrameSlot::new());
                // Décodage matériel par AVFoundation ; le son va au bus d'habillage (routé
                // vers la carte), ou est ignoré sans sortie audio.
                let player = avf::spawn(
                    &file,
                    *looped,
                    slot.clone(),
                    bus.map(|b| b.add_input()),
                    cfg.audio.sample_rate as u32,
                )?;
                keepalive.push(Box::new(player));
                Layer {
                    id,
                    kind: LayerKind::Video(slot),
                    geometry: geometry.clone(),
                    opacity: *opacity as f32,
                }
            }
        })
    }

    /// Met en place l'audio de la carte (CoreAudio via cpal) : sortie N canaux avec mixage
    /// stream + habillage sur des canaux distincts, entrée dont 2 canaux sont renvoyés au
    /// téléphone. Les VU-mètres sont calculés dans les callbacks.
    fn build_audio(cfg: &Config) -> Result<AudioSetup> {
        let a = &cfg.audio;
        let rate = a.sample_rate as u32;
        let mut setup = AudioSetup {
            route: AudioRoute {
                stream: a.stream_output_channels.clone(),
                branding: a.branding_output_channels.clone(),
                return_input: a.return_from_input_channels.clone(),
                out_channels: 0,
                in_channels: 0,
            },
            ..Default::default()
        };
        let pair = |chans: &[usize]| {
            let c0 = chans.first().copied().unwrap_or(1);
            let c1 = chans.get(1).copied().unwrap_or(c0);
            Arc::new([
                AtomicUsize::new(c0.saturating_sub(1)),
                AtomicUsize::new(c1.saturating_sub(1)),
            ])
        };
        let meters_arc = Arc::new(audio::cpal_out::Meters::default());

        // ----- Sortie vers la carte son -----
        let out_sel = a.output_device.trim();
        if !out_sel.is_empty() && !out_sel.eq_ignore_ascii_case("none") {
            match audio::cpal_out::best_config(out_sel) {
                Ok((dev_ch, _sr)) => {
                    let out_ch = if a.output_channels > 0 {
                        a.output_channels
                    } else {
                        dev_ch as i32
                    };
                    let stream_ch = pair(&a.stream_output_channels);
                    let branding_ch = pair(&a.branding_output_channels);
                    match audio::cpal_out::start_output_mixed(
                        out_sel,
                        out_ch as u16,
                        rate,
                        stream_ch.clone(),
                        branding_ch.clone(),
                        meters_arc.clone(),
                    ) {
                        Ok((output, phone_pusher, branding_pusher)) => {
                            setup.ctl.out_channels = out_ch;
                            setup.ctl.stream_ch = Some(stream_ch);
                            setup.ctl.branding_ch = Some(branding_ch);
                            setup.ctl.phone_pusher = Some(phone_pusher);
                            setup.ctl.meters_cpal = Some(meters_arc.clone());
                            setup.branding = Some(branding_pusher);
                            setup.route.out_channels = out_ch;
                            setup.keepalives.push(Box::new(output));
                            setup
                                .meters
                                .push(("stream".to_string(), "Stream (téléphone)".to_string()));
                            setup
                                .meters
                                .push(("branding".to_string(), "Habillage".to_string()));
                            info!(
                                "audio sortie « {out_sel} » via CoreAudio ({out_ch} canaux) : \
                                 habillage→{:?}, stream→{:?}",
                                a.branding_output_channels, a.stream_output_channels
                            );
                        }
                        Err(e) => warn!("sortie CoreAudio « {out_sel} » indisponible : {e:#}"),
                    }
                }
                Err(e) => warn!("sortie CoreAudio « {} » : {e:#}", a.output_device),
            }
        }

        // ----- Retour de la carte son → téléphone -----
        let sel_in = a.input_device.trim();
        if !sel_in.is_empty() && !sel_in.eq_ignore_ascii_case("none") {
            match audio::cpal_out::best_input_config(sel_in) {
                Ok((in_ch, _in_rate)) => {
                    let sel = pair(&a.return_from_input_channels);
                    match audio::cpal_out::start_input(
                        sel_in,
                        in_ch,
                        rate,
                        sel.clone(),
                        meters_arc.clone(),
                    ) {
                        Ok((input, reader)) => {
                            // Le retour est encodé en Opus (libopus) par la session webrtc-rs,
                            // qui lit ce `reader` ; le VU-mètre est calculé dans le callback.
                            setup.ctl.in_channels = in_ch as i32;
                            setup.ctl.return_sel = Some(sel);
                            setup.ctl.return_reader = Some(reader);
                            setup.ctl.meters_cpal = Some(meters_arc.clone());
                            setup.route.in_channels = in_ch as i32;
                            setup.keepalives.push(Box::new(input));
                            setup
                                .meters
                                .push(("return".to_string(), "Retour Wing".to_string()));
                            info!(
                                "audio retour « {sel_in} » via CoreAudio : {in_ch} canaux, entrées {:?} (Opus direct)",
                                a.return_from_input_channels
                            );
                        }
                        Err(e) => warn!("entrée CoreAudio « {sel_in} » indisponible : {e:#}"),
                    }
                }
                Err(e) => warn!("entrée CoreAudio « {sel_in} » : {e:#}"),
            }
        }

        Ok(setup)
    }

    /// Change à chaud les canaux d'une source audio (atomics lus dans les callbacks CoreAudio).
    pub fn set_audio_route(&self, target: &str, channels: &[usize]) -> bool {
        use std::sync::atomic::Ordering;
        let ctl = &self.audio_ctl;
        let sel = match target {
            "stream" => ctl.stream_ch.as_ref(),
            "branding" => ctl.branding_ch.as_ref(),
            "return" => ctl.return_sel.as_ref(),
            _ => None,
        };
        let Some(sel) = sel else { return false };
        let c0 = channels.first().copied().unwrap_or(1);
        let c1 = channels.get(1).copied().unwrap_or(c0);
        sel[0].store(c0.saturating_sub(1), Ordering::Relaxed);
        sel[1].store(c1.saturating_sub(1), Ordering::Relaxed);
        let mut route = self.route.lock().unwrap();
        match target {
            "stream" => route.stream = channels.to_vec(),
            "branding" => route.branding = channels.to_vec(),
            "return" => route.return_input = channels.to_vec(),
            _ => {}
        }
        info!("audio : {target} re-routé vers {channels:?}");
        true
    }

    pub fn audio_routing(&self) -> AudioRoute {
        self.route.lock().unwrap().clone()
    }

    /// Niveaux (VU-mètres), calculés dans les callbacks CoreAudio.
    pub fn meters(&self) -> Vec<AudioMeter> {
        let m = self.audio_ctl.meters_cpal.as_ref();
        self.meter_order
            .iter()
            .map(|(id, label)| {
                let (rms_db, peak_db) = match m {
                    Some(m) => {
                        let src = match id.as_str() {
                            "branding" => &m.branding,
                            "return" => &m.ret,
                            _ => &m.stream,
                        };
                        src.read_db()
                    }
                    None => (vec![-100.0; 2], vec![-100.0; 2]),
                };
                AudioMeter {
                    id: id.clone(),
                    label: label.clone(),
                    rms_db,
                    peak_db,
                }
            })
            .collect()
    }

    // ------------------------------------------------------------------------------------------
    // Contrôle
    // ------------------------------------------------------------------------------------------

    /// Les sources (flux CoreAudio, lecteurs de fichiers) tournent dès la construction.
    pub fn start(&self) -> Result<()> {
        info!(
            "moteur démarré (canevas {}x{})",
            self.cfg.video.width, self.cfg.video.height
        );
        Ok(())
    }

    /// Arrêt propre : ferme les flux CoreAudio (sinon une carte USB peut rester coincée jusqu'au
    /// rebranchement) et arrête les lecteurs de fichiers et le bus d'habillage (threads joints).
    pub fn stop(&self) {
        self.keepalive.lock().unwrap().clear();
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn scenes_ref(&self) -> &[Scene] {
        &self.scenes
    }

    pub fn phone_slot(&self) -> &Arc<FrameSlot> {
        &self.phone
    }

    /// Injecte l'audio du téléphone décodé par libopus (F32 stéréo 48 kHz) dans le mixeur cpal.
    pub fn push_phone_audio_f32(&self, samples: &[f32]) {
        if let Some(p) = &self.audio_ctl.phone_pusher {
            p.push(samples);
        }
    }

    /// Lecteur du retour (entrée carte, F32 stéréo) pour l'encodage Opus côté webrtc-rs.
    pub fn return_reader(&self) -> Option<audio::cpal_out::Reader> {
        self.audio_ctl.return_reader.clone()
    }

    pub fn scenes(&self) -> Vec<SceneInfo> {
        self.scenes
            .iter()
            .map(|s| SceneInfo {
                id: s.id.clone(),
                name: s.name.clone(),
            })
            .collect()
    }

    pub fn scene_index(&self, id: &str) -> Option<usize> {
        self.scenes.iter().position(|s| s.id == id)
    }

    fn scene_has_phone(&self, idx: usize) -> bool {
        self.scenes
            .get(idx)
            .map(|s| s.layers.iter().any(|l| matches!(l.kind, LayerKind::Phone)))
            .unwrap_or(false)
    }

    /// Le flux du téléphone est-il visible sur la sortie programme ? Vrai aussi pendant un
    /// fondu si la scène qui s'efface contient le téléphone (elle reste à l'antenne).
    pub fn phone_on_air(&self) -> bool {
        let st = self.state.lock().unwrap();
        let mut on = self.scene_has_phone(st.program);
        if let Some(t) = st.transition {
            if t.progress() < 1.0 {
                on = on || self.scene_has_phone(t.from);
            }
        }
        on
    }

    pub fn program_index(&self) -> usize {
        self.state.lock().unwrap().program
    }

    pub fn preview_index(&self) -> usize {
        self.state.lock().unwrap().preview
    }

    pub fn stats(&self) -> Stats {
        let (w, h) = self.phone.dimensions().unwrap_or((0, 0));
        Stats {
            phone_fps: self.phone.fps.value(),
            phone_width: w,
            phone_height: h,
            render_fps: self.render_fps.value(),
            rtp: self.rtp_stats.lock().unwrap().clone(),
            phone: self.phone_stats.lock().unwrap().clone(),
        }
    }

    pub fn set_rtp_stats(&self, st: Option<RtpStats>) {
        *self.rtp_stats.lock().unwrap() = st;
    }

    pub fn set_phone_stats(&self, st: Option<PhoneStats>) {
        *self.phone_stats.lock().unwrap() = st;
    }

    pub fn snapshot(&self) -> Snapshot {
        let st = self.state.lock().unwrap();
        Snapshot {
            scenes: self.scenes(),
            program: self.scenes[st.program].id.clone(),
            preview: self.scenes[st.preview].id.clone(),
            phone_connected: st.phone.is_some(),
            phone_name: st.phone.clone(),
            transition: self.cfg.transition.kind.clone(),
            stats: self.stats(),
        }
    }

    /// État instantané pour le rendu (termine les transitions échues).
    pub fn render_state(&self) -> RenderState {
        let mut st = self.state.lock().unwrap();
        let fade = match st.transition {
            Some(t) => {
                let p = t.progress();
                if p >= 1.0 {
                    st.transition = None;
                    None
                } else {
                    Some((t.from, p))
                }
            }
            None => None,
        };
        RenderState {
            program: st.program,
            preview: st.preview,
            fade,
            phone_connected: st.phone.is_some(),
        }
    }

    pub fn set_preview(&self, idx: usize) {
        if idx >= self.scenes.len() {
            return;
        }
        {
            let mut st = self.state.lock().unwrap();
            if st.preview == idx {
                return;
            }
            st.preview = idx;
        }
        let _ = self.events.send(Event::Preview {
            scene: self.scenes[idx].id.clone(),
        });
    }

    /// Preview → programme (avec la transition configurée) ; l'ancien programme devient la preview.
    pub fn take(&self) {
        let (prog, prev) = {
            let st = self.state.lock().unwrap();
            (st.program, st.preview)
        };
        self.switch_program(prev, true);
        self.set_preview(prog);
    }

    /// Bascule immédiate.
    pub fn cut(&self, idx: usize) {
        self.switch_program(idx, false);
    }

    /// Bascule avec la transition configurée.
    pub fn transition_to(&self, idx: usize) {
        self.switch_program(idx, true);
    }

    fn switch_program(&self, idx: usize, animated: bool) {
        if idx >= self.scenes.len() {
            return;
        }
        let duration = self.cfg.transition.duration_ms;
        let fade =
            animated && self.cfg.transition.kind.eq_ignore_ascii_case("fade") && duration > 0;
        {
            let mut st = self.state.lock().unwrap();
            if st.program == idx {
                return;
            }
            let from = st.program;
            st.program = idx;
            st.transition = if fade {
                Some(Transition {
                    from,
                    start: Instant::now(),
                    duration: Duration::from_millis(duration),
                })
            } else {
                None
            };
        }
        let _ = self.events.send(Event::Program {
            scene: self.scenes[idx].id.clone(),
        });
    }

    /// Notifie la connexion/déconnexion du téléphone.
    pub fn set_phone(&self, name: Option<String>) {
        {
            let mut st = self.state.lock().unwrap();
            st.phone = name.clone();
        }
        if name.is_none() {
            self.phone.clear();
            self.set_rtp_stats(None);
            self.set_phone_stats(None);
        }
        let _ = self.events.send(Event::Phone {
            connected: name.is_some(),
            name,
        });
    }
}

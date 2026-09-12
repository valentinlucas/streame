//! Moteur : état de la régie (scènes, programme, preview, transitions), sources
//! d'images et pipeline GStreamer d'appoint (audio, fichiers vidéo).
//!
//! La composition et l'affichage sont faits sur le GPU (voir `render.rs`) : GStreamer ne
//! sert plus qu'à recevoir/décoder le téléphone, lire les fichiers et router l'audio.
//!
//! ```text
//! pipeline WebRTC ─ vtdec ─ appsink ─► FrameSlot(phone) ─┐
//! uridecodebin (overlay.mp4) ─ appsink ─► FrameSlot(vidéo) ─┼─► rendu wgpu/Metal ─► HDMI + multiview
//! PNG / texte (décodés au démarrage) ────────────────────────┘
//! pipeline WebRTC ─ décode audio ─ appsink ─► appsrc(leaky) ─ mix-matrix ─┐
//! habillage (vidéos) ─ mix-matrix ─────────────────────────────────────────┼─► audiomixer ─► appsink ─► CoreAudio out (Wing)
//! CoreAudio in (Wing) ─► appsrc ─► interaudiosink(return) ─► pipeline WebRTC ─► opus ─► téléphone
//! ```
//! Toute l'E/S de la carte passe par CoreAudio (cpal) : GStreamer ne touche plus le
//! périphérique, ce qui évite le conflit à deux frameworks qui coinçait la Wing.

use crate::audio;
use crate::config::{parse_color, Config, Geometry, LayerConfig};
use crate::frame::{FpsCounter, FrameSlot};
use crate::text;
use anyhow::{Context, Result};
use gst::prelude::*;
use image::RgbaImage;
use serde::Serialize;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

/// Canaux audio inter-pipelines (partagés avec le pipeline WebRTC).
pub const RETURN_AUDIO_CHANNEL: &str = "streame-return-audio";

pub const NO_PHONE_TEXT: &str = "EN ATTENTE DU TÉLÉPHONE";

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

/// Statistiques RTP côté Mac (webrtcbin `get-stats`, flux vidéo entrant).
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

/// Éléments `audioconvert` (mix-matrix) modifiables à chaud pour re-router en direct.
#[derive(Default, Clone)]
struct AudioCtl {
    stream_conv: Option<gst::Element>,
    branding_conv: Option<gst::Element>,
    return_conv: Option<gst::Element>,
    /// Sélection des 2 canaux de retour quand l'entrée passe par CoreAudio (indices 0-based).
    return_sel: Option<Arc<[std::sync::atomic::AtomicUsize; 2]>>,
    out_channels: i32,
    in_channels: i32,
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
    pipeline: gst::Pipeline,
    scenes: Vec<Scene>,
    phone: Arc<FrameSlot>,
    state: Mutex<State>,
    events: broadcast::Sender<Event>,
    pub render_fps: FpsCounter,
    rtp_stats: Mutex<Option<RtpStats>>,
    phone_stats: Mutex<Option<PhoneStats>>,
    meters: Mutex<std::collections::HashMap<String, AudioMeter>>,
    meter_order: Vec<(String, String)>,
    audio_ctl: AudioCtl,
    route: Mutex<AudioRoute>,
    /// Objets maintenus en vie (flux de sortie CoreAudio) tant que le moteur existe.
    _audio_keepalive: Mutex<Vec<Box<dyn std::any::Any + Send>>>,
    /// Point d'injection du son du téléphone (rempli par la session WebRTC).
    phone_audio_src: Option<gst_app::AppSrc>,
}

fn make(factory: &str) -> Result<gst::Element> {
    gst::ElementFactory::make(factory).build().with_context(|| {
        format!("élément GStreamer « {factory} » indisponible (plugin manquant ?)")
    })
}

fn capsfilter(caps: &gst::Caps) -> Result<gst::Element> {
    Ok(gst::ElementFactory::make("capsfilter")
        .property("caps", caps)
        .build()?)
}

fn link_many(els: &[&gst::Element]) -> Result<()> {
    for w in els.windows(2) {
        w[0].link(w[1])
            .with_context(|| format!("liaison {} → {}", w[0].name(), w[1].name()))?;
    }
    Ok(())
}

/// File audio courte (limite la latence de bout en bout).
fn audio_queue() -> Result<gst::Element> {
    Ok(gst::ElementFactory::make("queue")
        .property("max-size-time", 200u64 * gst::ClockTime::MSECOND.nseconds())
        .property("max-size-buffers", 0u32)
        .property("max-size-bytes", 0u32)
        .build()?)
}

/// Élément `level` (VU-mètre) nommé, ou `identity` si les mètres sont désactivés.
fn level_element(name: &str, enabled: bool) -> Result<gst::Element> {
    if !enabled {
        return make("identity");
    }
    Ok(gst::ElementFactory::make("level")
        .name(name)
        .property("post-messages", true)
        .property("interval", 50u64 * gst::ClockTime::MSECOND.nseconds())
        .property("peak-ttl", 300u64 * gst::ClockTime::MSECOND.nseconds())
        .property("peak-falloff", 20.0f64)
        .build()?)
}

/// Extrait un tableau de dB (rms/peak) d'un message `level`.
fn parse_level_array(s: &gst::StructureRef, field: &str) -> Vec<f32> {
    s.get::<gst::glib::ValueArray>(field)
        .ok()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.get::<f64>().ok().map(|x| x as f32))
                .collect()
        })
        .unwrap_or_default()
}

/// Caps acceptées par le rendu GPU (NV12 de préférence : sortie native des décodeurs).
pub fn gpu_caps() -> gst::Caps {
    gst::Caps::builder("video/x-raw")
        .field("format", gst::List::new(["NV12", "RGBA", "BGRA"]))
        .build()
}

/// `appsink` qui pousse chaque image dans un `FrameSlot`.
pub fn frame_appsink(slot: Arc<FrameSlot>, sync: bool) -> gst_app::AppSink {
    let sink = gst_app::AppSink::builder()
        .caps(&gpu_caps())
        .drop(true)
        .max_buffers(1)
        .sync(sync)
        .build();
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |s| {
                let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                slot.push(sample);
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    sink
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
        let pipeline = gst::Pipeline::with_name("streame");
        let mut next_layer_id = 1u64;

        // Graphe audio d'abord : le mélangeur d'habillage doit exister avant les vidéos,
        // dont le son s'y branche.
        let (branding_mixer, audio_ctl, meter_order, route, audio_keepalive, phone_audio_src) =
            if cfg.audio.enabled {
                match Self::build_audio(&cfg, &pipeline) {
                    Ok(x) => x,
                    Err(e) => {
                        error!("audio désactivé : {e:#}");
                        (
                            None,
                            AudioCtl::default(),
                            Vec::new(),
                            AudioRoute::default(),
                            Vec::new(),
                            None,
                        )
                    }
                }
            } else {
                (
                    None,
                    AudioCtl::default(),
                    Vec::new(),
                    AudioRoute::default(),
                    Vec::new(),
                    None,
                )
            };

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
                match Self::build_layer(&cfg, &pipeline, lc, next_layer_id, branding_mixer.as_ref())
                {
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

        let mut meters = std::collections::HashMap::new();
        for (id, label) in &meter_order {
            meters.insert(
                id.clone(),
                AudioMeter {
                    id: id.clone(),
                    label: label.clone(),
                    ..Default::default()
                },
            );
        }

        let engine = Arc::new(Engine {
            cfg: cfg.clone(),
            pipeline,
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
            meters: Mutex::new(meters),
            meter_order,
            audio_ctl,
            route: Mutex::new(route),
            _audio_keepalive: Mutex::new(audio_keepalive),
            phone_audio_src,
        });
        engine.spawn_bus_thread();
        Ok(engine)
    }

    fn build_layer(
        cfg: &Config,
        pipeline: &gst::Pipeline,
        lc: &LayerConfig,
        id: u64,
        branding_mixer: Option<&gst::Element>,
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
                Self::build_video_player(
                    pipeline,
                    &file,
                    *looped,
                    slot.clone(),
                    branding_mixer.cloned(),
                    cfg.audio.sample_rate,
                )?;
                Layer {
                    id,
                    kind: LayerKind::Video(slot),
                    geometry: geometry.clone(),
                    opacity: *opacity as f32,
                }
            }
        })
    }

    /// `uridecodebin` → `appsink` (+ boucle sans coupure par seek « segment »).
    /// Le son de la vidéo est envoyé au mélangeur d'habillage s'il existe, sinon ignoré.
    fn build_video_player(
        pipeline: &gst::Pipeline,
        file: &std::path::Path,
        looped: bool,
        slot: Arc<FrameSlot>,
        branding_mixer: Option<gst::Element>,
        sample_rate: i32,
    ) -> Result<()> {
        let file = std::fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
        let uri = gst::glib::filename_to_uri(&file, None).context("uri du fichier vidéo")?;
        let dec = gst::ElementFactory::make("uridecodebin")
            .property("uri", uri.as_str())
            .build()?;
        let q = gst::ElementFactory::make("queue")
            .property("max-size-buffers", 3u32)
            .property("max-size-time", 0u64)
            .property("max-size-bytes", 0u32)
            .build()?;
        let convert = make("videoconvert")?;
        let cf = capsfilter(&gpu_caps())?;
        let sink = frame_appsink(slot, true);
        pipeline.add_many([&dec, &q, &convert, &cf, sink.upcast_ref()])?;
        link_many(&[&q, &convert, &cf, sink.upcast_ref()])?;
        let q_sink = q.static_pad("sink").unwrap();
        let pipe = pipeline.downgrade();
        dec.connect_pad_added(move |dec, dpad| {
            let Some(caps) = dpad.current_caps().or_else(|| dpad.allowed_caps()) else {
                return;
            };
            let Some(s) = caps.structure(0) else { return };
            if s.name().starts_with("video/") {
                if let Err(e) = dpad.link(&q_sink) {
                    warn!("vidéo : liaison decodebin impossible : {e:?}");
                }
                if looped {
                    Self::setup_loop(dec, dpad);
                }
            } else if s.name().starts_with("audio/") {
                let Some(p) = pipe.upgrade() else { return };
                match &branding_mixer {
                    // Son de l'habillage → mélangeur d'habillage (routé vers la Wing).
                    Some(bmix) => {
                        if let Err(e) = Self::link_branding_audio(&p, dpad, bmix, sample_rate) {
                            warn!("son d'habillage non branché : {e:#}");
                        }
                    }
                    // Pas de sortie audio : on jette le son.
                    None => {
                        if let Ok(sink) = gst::ElementFactory::make("fakesink")
                            .property("sync", true)
                            .build()
                        {
                            let _ = p.add(&sink);
                            let _ = sink.sync_state_with_parent();
                            let _ = dpad.link(&sink.static_pad("sink").unwrap());
                        }
                    }
                }
            }
        });
        Ok(())
    }

    /// Boucle sans coupure : seek « segment » initial (flushant), puis à chaque événement
    /// segment-done du démultiplexeur, un seek segment non flushant (temps continu).
    /// L'événement est intercepté par branche : le pipeline agrège les messages du bus.
    fn setup_loop(dec: &gst::Element, dpad: &gst::Pad) {
        let dec2 = dec.clone();
        dpad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
            if let Some(gst::PadProbeData::Event(ev)) = &info.data {
                if ev.type_() == gst::EventType::SegmentDone {
                    let dec = dec2.clone();
                    thread::spawn(move || {
                        debug!("boucle vidéo : {}", dec.name());
                        if let Err(e) = dec.seek(
                            1.0,
                            gst::SeekFlags::SEGMENT,
                            gst::SeekType::Set,
                            gst::ClockTime::ZERO,
                            gst::SeekType::None,
                            gst::ClockTime::NONE,
                        ) {
                            warn!("boucle vidéo : seek impossible : {e}");
                        }
                    });
                }
            }
            gst::PadProbeReturn::Ok
        });
        let dec = dec.clone();
        let dpad = dpad.clone();
        thread::spawn(move || {
            // Après un seek flushant sur cette branche seule, le temps de lecture repart à
            // zéro : on compense avec un décalage de pad égal au temps courant du pipeline.
            let offset = dec
                .parent()
                .and_then(|p| p.downcast::<gst::Pipeline>().ok())
                .and_then(|p| {
                    let base = p.base_time()?;
                    let now = p.clock()?.time();
                    now.checked_sub(base)
                })
                .unwrap_or(gst::ClockTime::ZERO);
            if let Err(e) = dec.seek(
                1.0,
                gst::SeekFlags::FLUSH | gst::SeekFlags::SEGMENT,
                gst::SeekType::Set,
                gst::ClockTime::ZERO,
                gst::SeekType::None,
                gst::ClockTime::NONE,
            ) {
                warn!(
                    "boucle vidéo : seek initial impossible sur {} : {e}",
                    dec.name()
                );
                return;
            }
            dpad.set_offset(offset.nseconds() as i64);
            debug!(
                "boucle vidéo {} : segment initial (décalage {offset})",
                dec.name()
            );
        });
    }

    /// Branche le son d'une vidéo d'habillage sur le mélangeur d'habillage.
    fn link_branding_audio(
        pipeline: &gst::Pipeline,
        dpad: &gst::Pad,
        bmix: &gst::Element,
        rate: i32,
    ) -> Result<()> {
        let q = audio_queue()?;
        let conv = make("audioconvert")?;
        let rs = make("audioresample")?;
        let cf = capsfilter(&audio::raw_caps(rate, 2))?;
        pipeline.add_many([&q, &conv, &rs, &cf])?;
        link_many(&[&q, &conv, &rs, &cf])?;
        let bpad = bmix
            .request_pad_simple("sink_%u")
            .context("pad mélangeur habillage")?;
        cf.static_pad("src").unwrap().link(&bpad)?;
        for el in [&q, &conv, &rs, &cf] {
            el.sync_state_with_parent()?;
        }
        dpad.link(&q.static_pad("sink").unwrap())?;
        Ok(())
    }

    /// Construit le graphe audio. Deux sources (stream WebRTC et habillage) sont mélangées
    /// vers la carte son sur des canaux distincts ; l'entrée est renvoyée au téléphone.
    /// Retourne le mélangeur d'habillage (pour y brancher le son des vidéos), les éléments
    /// re-routables à chaud, l'ordre des VU-mètres et le routage courant.
    #[allow(clippy::type_complexity)]
    fn build_audio(
        cfg: &Config,
        pipeline: &gst::Pipeline,
    ) -> Result<(
        Option<gst::Element>,
        AudioCtl,
        Vec<(String, String)>,
        AudioRoute,
        Vec<Box<dyn std::any::Any + Send>>,
        Option<gst_app::AppSrc>,
    )> {
        let a = &cfg.audio;
        let rate = a.sample_rate;
        let mut ctl = AudioCtl::default();
        let mut branding_mixer = None;
        let mut phone_audio_src: Option<gst_app::AppSrc> = None;
        let mut meters = Vec::new();
        // Objets à garder en vie tant que le moteur tourne (flux CoreAudio de sortie).
        let mut keepalives: Vec<Box<dyn std::any::Any + Send>> = Vec::new();
        let mut route = AudioRoute {
            stream: a.stream_output_channels.clone(),
            branding: a.branding_output_channels.clone(),
            return_input: a.return_from_input_channels.clone(),
            out_channels: 0,
            in_channels: 0,
        };

        // ----- Sortie vers la carte son (Wing) -----
        if let Some((out_ch, sink_head, keepalive)) = Self::build_output_sink(pipeline, cfg)? {
            let max_route = a
                .stream_output_channels
                .iter()
                .chain(a.branding_output_channels.iter())
                .copied()
                .max()
                .unwrap_or(0) as i32;
            if max_route > out_ch {
                warn!(
                    "audio sortie « {} » : routage vers le canal {max_route} mais la sortie n'a \
                     que {out_ch} canaux.",
                    a.output_device
                );
            }
            if let Some(k) = keepalive {
                keepalives.push(k);
            }
            ctl.out_channels = out_ch;
            route.out_channels = out_ch;

            let out_mixer = make("audiomixer")?;
            pipeline.add(&out_mixer)?;
            out_mixer
                .link(&sink_head)
                .context("liaison mélangeur sortie → sortie audio")?;

            // Branche STREAM (téléphone WebRTC) → canaux stream_output_channels.
            // Le son du téléphone (autre pipeline, autre horloge) arrive par un appsrc « leaky » :
            // si l'horloge du téléphone dérive, on jette le plus ancien au lieu de laisser la
            // latence grossir (le pont interaudiosink/src accumulait sinon plusieurs secondes).
            {
                let stream_caps = gst::Caps::builder("audio/x-raw")
                    .field("format", "F32LE")
                    .field("layout", "interleaved")
                    .field("rate", rate)
                    .field("channels", 2)
                    .build();
                let src = gst_app::AppSrc::builder()
                    .caps(&stream_caps)
                    .is_live(true)
                    .format(gst::Format::Time)
                    .do_timestamp(true)
                    // Latence bornée : sinon la source live répond « illimité » à la requête de
                    // latence et le mélangeur (aggregator) échoue avec une erreur d'horloge.
                    .min_latency(0)
                    .max_latency(150_000_000) // 150 ms en ns
                    .max_time(gst::ClockTime::from_mseconds(150))
                    .leaky_type(gst_app::AppLeakyType::Downstream)
                    .build();
                let c1 = make("audioconvert")?;
                let r1 = make("audioresample")?;
                let cf1 = capsfilter(&audio::raw_caps(rate, 2))?;
                let level = level_element("level_stream", a.meters)?;
                let matrix = audio::route_matrix(2, out_ch as usize, &a.stream_output_channels);
                let conv = gst::ElementFactory::make("audioconvert")
                    .property("mix-matrix", audio::to_gst_matrix(&matrix))
                    .build()?;
                let cf2 = capsfilter(&audio::raw_caps(rate, out_ch))?;
                pipeline.add_many([src.upcast_ref(), &c1, &r1, &cf1, &level, &conv, &cf2])?;
                link_many(&[src.upcast_ref(), &c1, &r1, &cf1, &level, &conv, &cf2])?;
                let mpad = out_mixer
                    .request_pad_simple("sink_%u")
                    .context("pad mélangeur sortie (stream)")?;
                cf2.static_pad("src").unwrap().link(&mpad)?;
                ctl.stream_conv = Some(conv);
                phone_audio_src = Some(src);
                meters.push(("stream".to_string(), "Stream (téléphone)".to_string()));
            }

            // Branche HABILLAGE → canaux branding_output_channels.
            {
                let bmix = make("audiomixer")?;
                // Source de silence : garde la branche active même sans vidéo d'habillage.
                let silence = gst::ElementFactory::make("audiotestsrc")
                    .property("is-live", true)
                    .property_from_str("wave", "silence")
                    .build()?;
                let silence_caps = capsfilter(&audio::raw_caps(rate, 2))?;
                pipeline.add_many([&bmix, &silence, &silence_caps])?;
                link_many(&[&silence, &silence_caps])?;
                let bpad0 = bmix
                    .request_pad_simple("sink_%u")
                    .context("pad silence habillage")?;
                silence_caps.static_pad("src").unwrap().link(&bpad0)?;

                let bcaps = capsfilter(&audio::raw_caps(rate, 2))?;
                let level = level_element("level_branding", a.meters)?;
                let matrix = audio::route_matrix(2, out_ch as usize, &a.branding_output_channels);
                let conv = gst::ElementFactory::make("audioconvert")
                    .property("mix-matrix", audio::to_gst_matrix(&matrix))
                    .build()?;
                let cf2 = capsfilter(&audio::raw_caps(rate, out_ch))?;
                pipeline.add_many([&bcaps, &level, &conv, &cf2])?;
                link_many(&[&bmix, &bcaps, &level, &conv, &cf2])?;
                let mpad = out_mixer
                    .request_pad_simple("sink_%u")
                    .context("pad mélangeur sortie (habillage)")?;
                cf2.static_pad("src").unwrap().link(&mpad)?;
                ctl.branding_conv = Some(conv);
                branding_mixer = Some(bmix);
                meters.push(("branding".to_string(), "Habillage".to_string()));
            }

            info!(
                "audio sortie « {} » ({} canaux) : habillage→{:?}, stream→{:?}",
                a.output_device, out_ch, a.branding_output_channels, a.stream_output_channels
            );
        }

        // ----- Retour de la carte son → téléphone -----
        // Sur macOS avec une carte nommée, l'entrée passe aussi par CoreAudio (cpal) : la même
        // carte n'est ainsi ouverte que par un seul framework (plus de conflit avec osxaudiosrc).
        let sel_in = a.input_device.trim();
        let mut return_done = false;
        #[cfg(target_os = "macos")]
        if !sel_in.is_empty()
            && !sel_in.eq_ignore_ascii_case("none")
            && !sel_in.eq_ignore_ascii_case("default")
        {
            match audio::cpal_out::best_input_config(sel_in) {
                Ok((in_ch, _in_rate)) => {
                    use std::sync::atomic::AtomicUsize;
                    let ch0 = a.return_from_input_channels.first().copied().unwrap_or(1);
                    let ch1 = a.return_from_input_channels.get(1).copied().unwrap_or(ch0);
                    let sel = Arc::new([
                        AtomicUsize::new(ch0.saturating_sub(1)),
                        AtomicUsize::new(ch1.saturating_sub(1)),
                    ]);
                    match audio::cpal_out::start_input(sel_in, in_ch, rate as u32, sel.clone()) {
                        Ok((input, reader)) => {
                            let caps = gst::Caps::builder("audio/x-raw")
                                .field("format", "F32LE")
                                .field("layout", "interleaved")
                                .field("rate", rate)
                                .field("channels", 2i32)
                                .build();
                            let appsrc = gst_app::AppSrc::builder()
                                .caps(&caps)
                                .is_live(true)
                                .format(gst::Format::Time)
                                .do_timestamp(true)
                                .min_latency(0)
                                .max_latency(150_000_000)
                                .leaky_type(gst_app::AppLeakyType::Downstream)
                                .max_time(gst::ClockTime::from_mseconds(150))
                                .build();
                            let chunk = (rate as usize / 100).max(1); // 10 ms de trames stéréo
                            appsrc.set_callbacks(
                                gst_app::AppSrcCallbacks::builder()
                                    .need_data(move |src, _| {
                                        let mut buf = vec![0f32; chunk * 2];
                                        reader.pull(&mut buf); // le reste non fourni reste silence
                                        let bytes: Vec<u8> = bytemuck::cast_slice(&buf).to_vec();
                                        let _ = src.push_buffer(gst::Buffer::from_mut_slice(bytes));
                                    })
                                    .build(),
                            );
                            let level = level_element("level_return", a.meters)?;
                            let sink = gst::ElementFactory::make("interaudiosink")
                                .property("channel", RETURN_AUDIO_CHANNEL)
                                .build()?;
                            pipeline.add_many([appsrc.upcast_ref(), &level, &sink])?;
                            link_many(&[appsrc.upcast_ref(), &level, &sink])?;
                            ctl.in_channels = in_ch as i32;
                            ctl.return_sel = Some(sel);
                            route.in_channels = in_ch as i32;
                            keepalives.push(Box::new(input));
                            meters.push(("return".to_string(), "Retour Wing".to_string()));
                            info!(
                                "audio retour « {sel_in} » via CoreAudio : {in_ch} canaux, entrées {:?}",
                                a.return_from_input_channels
                            );
                            return_done = true;
                        }
                        Err(e) => warn!(
                            "entrée CoreAudio « {sel_in} » indisponible ({e:#}) ; repli GStreamer"
                        ),
                    }
                }
                Err(e) => warn!("entrée CoreAudio « {sel_in} » : {e:#} ; repli GStreamer"),
            }
        }

        if !return_done {
            if let Some((src, in_ch)) = audio::make_device_element(
                &a.input_device,
                audio::Direction::Source,
                a.input_channels,
            )? {
                ctl.in_channels = in_ch;
                route.in_channels = in_ch;
                let cf0 = capsfilter(
                    &gst::Caps::builder("audio/x-raw")
                        .field("channels", in_ch)
                        .build(),
                )?;
                let matrix = audio::select_matrix(in_ch as usize, 2, &a.return_from_input_channels);
                let conv = gst::ElementFactory::make("audioconvert")
                    .property("mix-matrix", audio::to_gst_matrix(&matrix))
                    .build()?;
                let cf1 = capsfilter(&audio::raw_caps(rate, 2))?;
                let level = level_element("level_return", a.meters)?;
                let r1 = make("audioresample")?;
                let q = audio_queue()?;
                let sink = gst::ElementFactory::make("interaudiosink")
                    .property("channel", RETURN_AUDIO_CHANNEL)
                    .build()?;
                pipeline.add_many([&src, &cf0, &conv, &cf1, &level, &r1, &q, &sink])?;
                link_many(&[&src, &cf0, &conv, &cf1, &level, &r1, &q, &sink])?;
                ctl.return_conv = Some(conv);
                meters.push(("return".to_string(), "Retour Wing".to_string()));
                info!(
                    "audio retour « {} » ({} canaux) : entrées {:?} → téléphone",
                    a.input_device, in_ch, a.return_from_input_channels
                );
            }
        }

        Ok((
            branding_mixer,
            ctl,
            meters,
            route,
            keepalives,
            phone_audio_src,
        ))
    }

    /// Construit la fin de la chaîne de sortie audio. Sur macOS avec une carte nommée, la
    /// sortie passe par CoreAudio (cpal) car `osxaudiosink` se limite à 2 canaux : le graphe
    /// produit du F32 entrelacé à N canaux, poussé dans un `appsink` vers le flux CoreAudio.
    /// Retourne (nombre de canaux, élément d'entrée de la chaîne à relier au mélangeur,
    /// objet à garder en vie).
    #[allow(clippy::type_complexity)]
    fn build_output_sink(
        pipeline: &gst::Pipeline,
        cfg: &Config,
    ) -> Result<Option<(i32, gst::Element, Option<Box<dyn std::any::Any + Send>>)>> {
        let a = &cfg.audio;
        let rate = a.sample_rate;
        let sel = a.output_device.trim();
        if sel.is_empty() || sel.eq_ignore_ascii_case("none") {
            return Ok(None);
        }

        #[cfg(target_os = "macos")]
        if !sel.eq_ignore_ascii_case("default") {
            match audio::cpal_out::best_config(sel) {
                Ok((dev_ch, dev_rate)) => {
                    let ch = if a.output_channels > 0 {
                        a.output_channels
                    } else {
                        dev_ch as i32
                    };
                    let sr = dev_rate as i32;
                    match audio::cpal_out::start(sel, ch as u16, sr as u32) {
                        Ok((output, pusher)) => {
                            let caps = gst::Caps::builder("audio/x-raw")
                                .field("format", "F32LE")
                                .field("layout", "interleaved")
                                .field("rate", sr)
                                .field("channels", ch)
                                .field("channel-mask", gst::Bitmask::new(0))
                                .build();
                            let convert = make("audioconvert")?;
                            let resample = make("audioresample")?;
                            let out_caps = capsfilter(&caps)?;
                            let appsink = gst_app::AppSink::builder()
                                .caps(&caps)
                                .sync(false)
                                .max_buffers(4)
                                .drop(true)
                                .build();
                            appsink.set_callbacks(
                                gst_app::AppSinkCallbacks::builder()
                                    .new_sample(move |s| {
                                        let sample =
                                            s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                                        if let Some(buf) = sample.buffer() {
                                            if let Ok(map) = buf.map_readable() {
                                                let f: &[f32] =
                                                    bytemuck::cast_slice(map.as_slice());
                                                pusher.push(f);
                                            }
                                        }
                                        Ok(gst::FlowSuccess::Ok)
                                    })
                                    .build(),
                            );
                            pipeline.add_many([
                                &convert,
                                &resample,
                                &out_caps,
                                appsink.upcast_ref(),
                            ])?;
                            link_many(&[&convert, &resample, &out_caps, appsink.upcast_ref()])?;
                            info!("audio sortie « {sel} » via CoreAudio : {ch} canaux @ {sr} Hz");
                            return Ok(Some((ch, convert, Some(Box::new(output)))));
                        }
                        Err(e) => {
                            warn!("CoreAudio « {sel} » indisponible ({e:#}) ; repli GStreamer")
                        }
                    }
                }
                Err(e) => warn!("CoreAudio « {sel} » : {e:#} ; repli GStreamer"),
            }
        }

        // Repli (autres plateformes, « default », ou CoreAudio indisponible) : sink GStreamer.
        if let Some((sink, out_ch)) =
            audio::make_device_element(sel, audio::Direction::Sink, a.output_channels)?
        {
            let out_caps = capsfilter(&audio::raw_caps(rate, out_ch))?;
            let out_q = audio_queue()?;
            pipeline.add_many([&out_caps, &out_q, &sink])?;
            link_many(&[&out_caps, &out_q, &sink])?;
            return Ok(Some((out_ch, out_caps, None)));
        }
        Ok(None)
    }

    /// Change à chaud les canaux d'une source audio (mix-matrix de l'`audioconvert`).
    pub fn set_audio_route(&self, target: &str, channels: &[usize]) -> bool {
        let ctl = &self.audio_ctl;
        // Retour via CoreAudio : la sélection se fait par atomics dans le callback d'entrée.
        if target == "return" {
            if let Some(sel) = &ctl.return_sel {
                use std::sync::atomic::Ordering;
                let c0 = channels.first().copied().unwrap_or(1);
                let c1 = channels.get(1).copied().unwrap_or(c0);
                sel[0].store(c0.saturating_sub(1), Ordering::Relaxed);
                sel[1].store(c1.saturating_sub(1), Ordering::Relaxed);
                self.route.lock().unwrap().return_input = channels.to_vec();
                info!("audio : return re-routé vers {channels:?}");
                return true;
            }
        }
        let (conv, matrix) = match target {
            "stream" => (
                ctl.stream_conv.clone(),
                audio::route_matrix(2, ctl.out_channels as usize, channels),
            ),
            "branding" => (
                ctl.branding_conv.clone(),
                audio::route_matrix(2, ctl.out_channels as usize, channels),
            ),
            "return" => (
                ctl.return_conv.clone(),
                audio::select_matrix(ctl.in_channels as usize, 2, channels),
            ),
            _ => (None, Vec::new()),
        };
        let Some(conv) = conv else { return false };
        conv.set_property("mix-matrix", audio::to_gst_matrix(&matrix));
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

    pub fn meters(&self) -> Vec<AudioMeter> {
        let m = self.meters.lock().unwrap();
        self.meter_order
            .iter()
            .map(|(id, label)| {
                m.get(id).cloned().unwrap_or_else(|| AudioMeter {
                    id: id.clone(),
                    label: label.clone(),
                    rms_db: vec![-100.0; 2],
                    peak_db: vec![-100.0; 2],
                })
            })
            .collect()
    }

    fn spawn_bus_thread(self: &Arc<Self>) {
        let bus = self.pipeline.bus().expect("bus");
        let weak = Arc::downgrade(self);
        thread::Builder::new()
            .name("gst-bus".into())
            .spawn(move || {
                for msg in bus.iter_timed(gst::ClockTime::NONE) {
                    use gst::MessageView;
                    match msg.view() {
                        MessageView::Error(e) => {
                            error!(
                                "GStreamer [{}] : {} ({:?})",
                                e.src().map(|s| s.path_string()).unwrap_or_default(),
                                e.error(),
                                e.debug()
                            );
                        }
                        MessageView::Warning(w) => {
                            warn!(
                                "GStreamer [{}] : {}",
                                w.src().map(|s| s.path_string()).unwrap_or_default(),
                                w.error()
                            );
                        }
                        MessageView::Element(_) => {
                            if let (Some(engine), Some(s)) = (weak.upgrade(), msg.structure()) {
                                if s.name() == "level" {
                                    let id = msg.src().and_then(|src| match src.name().as_str() {
                                        "level_stream" => Some("stream"),
                                        "level_branding" => Some("branding"),
                                        "level_return" => Some("return"),
                                        _ => None,
                                    });
                                    if let Some(id) = id {
                                        let label = engine
                                            .meter_order
                                            .iter()
                                            .find(|(i, _)| i == id)
                                            .map(|(_, l)| l.clone())
                                            .unwrap_or_default();
                                        let meter = AudioMeter {
                                            id: id.to_string(),
                                            label,
                                            rms_db: parse_level_array(s, "rms"),
                                            peak_db: parse_level_array(s, "peak"),
                                        };
                                        engine.meters.lock().unwrap().insert(id.to_string(), meter);
                                    }
                                }
                            }
                        }
                        MessageView::Latency(_) => {
                            if let Some(engine) = weak.upgrade() {
                                let _ = engine.pipeline.recalculate_latency();
                            }
                        }
                        MessageView::StateChanged(s) => {
                            if let Some(engine) = weak.upgrade() {
                                if msg.src().map(|x| x.as_ptr() as usize)
                                    == Some(engine.pipeline.as_ptr() as usize)
                                {
                                    debug!("pipeline : {:?} → {:?}", s.old(), s.current());
                                }
                            }
                        }
                        _ => {}
                    }
                }
                debug!("thread bus terminé");
            })
            .expect("thread bus");
    }

    // ------------------------------------------------------------------------------------------
    // Contrôle
    // ------------------------------------------------------------------------------------------

    pub fn start(&self) -> Result<()> {
        self.pipeline
            .set_state(gst::State::Playing)
            .context("passage en PLAYING")?;
        info!(
            "moteur démarré (canevas {}x{})",
            self.cfg.video.width, self.cfg.video.height
        );
        Ok(())
    }

    pub fn stop(&self) {
        let _ = self.pipeline.set_state(gst::State::Null);
        if let Some(bus) = self.pipeline.bus() {
            bus.set_flushing(true);
        }
        // Ferme proprement les flux CoreAudio (sinon une carte USB peut rester coincée
        // jusqu'au rebranchement). Le Drop des objets arrête les flux et joint les threads.
        self._audio_keepalive.lock().unwrap().clear();
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

    /// Injecte un tampon de son du téléphone (F32 stéréo 48 kHz) dans la sortie audio.
    /// L'appsrc « leaky » borne la latence en cas de dérive d'horloge.
    pub fn push_phone_audio(&self, buffer: gst::Buffer) {
        if let Some(src) = &self.phone_audio_src {
            let _ = src.push_buffer(buffer);
        }
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

/// Vérifie la présence des éléments GStreamer nécessaires.
pub fn check_elements() -> Vec<&'static str> {
    const REQUIRED: &[&str] = &[
        "videoconvert",
        "capsfilter",
        "queue",
        "interaudiosrc",
        "interaudiosink",
        "audiomixer",
        "level",
        "audiotestsrc",
        "decodebin",
        "uridecodebin",
        "audioconvert",
        "audioresample",
        "webrtcbin",
        "opusenc",
        "opusdec",
        "rtpopuspay",
        "rtpopusdepay",
        "nicesrc",
        "dtlssrtpdec",
        "srtpdec",
        "appsink",
    ];
    REQUIRED
        .iter()
        .copied()
        .filter(|f| gst::ElementFactory::find(f).is_none())
        .collect()
}

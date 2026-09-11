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
//! interaudiosrc(phone) ─ audioconvert(mix-matrix) ─► carte son (Wing)
//! carte son (Wing) ─ audioconvert(mix-matrix) ─► interaudiosink(return) ─► téléphone
//! ```

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
pub const PHONE_AUDIO_CHANNEL: &str = "streame-phone-audio";
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
                match Self::build_layer(&cfg, &pipeline, lc, next_layer_id) {
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

        if cfg.audio.enabled {
            if let Err(e) = Self::build_audio(&cfg, &pipeline) {
                error!("audio désactivé : {e:#}");
            }
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
        });
        engine.spawn_bus_thread();
        Ok(engine)
    }

    fn build_layer(
        cfg: &Config,
        pipeline: &gst::Pipeline,
        lc: &LayerConfig,
        id: u64,
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
                Self::build_video_player(pipeline, &file, *looped, slot.clone())?;
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
    fn build_video_player(
        pipeline: &gst::Pipeline,
        file: &std::path::Path,
        looped: bool,
        slot: Arc<FrameSlot>,
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
                // Le son des vidéos d'overlay n'est pas diffusé.
                if let Some(p) = pipe.upgrade() {
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

    fn build_audio(cfg: &Config, pipeline: &gst::Pipeline) -> Result<()> {
        let a = &cfg.audio;
        // Téléphone → carte son
        if let Some((sink, out_ch)) =
            audio::make_device_element(&a.output_device, audio::Direction::Sink, a.output_channels)?
        {
            let src = gst::ElementFactory::make("interaudiosrc")
                .property("channel", PHONE_AUDIO_CHANNEL)
                .build()?;
            let c1 = make("audioconvert")?;
            let r1 = make("audioresample")?;
            let cf1 = capsfilter(&audio::raw_caps(a.sample_rate, 2))?;
            let matrix = audio::route_matrix(2, out_ch as usize, &a.phone_to_output_channels);
            let c2 = gst::ElementFactory::make("audioconvert")
                .property("mix-matrix", audio::to_gst_matrix(&matrix))
                .build()?;
            let cf2 = capsfilter(&audio::raw_caps(a.sample_rate, out_ch))?;
            let q = gst::ElementFactory::make("queue")
                .property("max-size-time", 200u64 * gst::ClockTime::MSECOND.nseconds())
                .build()?;
            pipeline.add_many([&src, &c1, &r1, &cf1, &c2, &cf2, &q, &sink])?;
            link_many(&[&src, &c1, &r1, &cf1, &c2, &cf2, &q, &sink])?;
            info!(
                "audio : téléphone → sortie canaux {:?} ({} canaux)",
                a.phone_to_output_channels, out_ch
            );
        }
        // Carte son → téléphone (retour)
        if let Some((src, in_ch)) =
            audio::make_device_element(&a.input_device, audio::Direction::Source, a.input_channels)?
        {
            let cf0 = capsfilter(
                &gst::Caps::builder("audio/x-raw")
                    .field("channels", in_ch)
                    .build(),
            )?;
            let matrix = audio::select_matrix(in_ch as usize, 2, &a.return_from_input_channels);
            let c1 = gst::ElementFactory::make("audioconvert")
                .property("mix-matrix", audio::to_gst_matrix(&matrix))
                .build()?;
            let cf1 = capsfilter(&audio::raw_caps(a.sample_rate, 2))?;
            let r1 = make("audioresample")?;
            let q = gst::ElementFactory::make("queue")
                .property("max-size-time", 200u64 * gst::ClockTime::MSECOND.nseconds())
                .build()?;
            let sink = gst::ElementFactory::make("interaudiosink")
                .property("channel", RETURN_AUDIO_CHANNEL)
                .build()?;
            pipeline.add_many([&src, &cf0, &c1, &cf1, &r1, &q, &sink])?;
            link_many(&[&src, &cf0, &c1, &cf1, &r1, &q, &sink])?;
            info!(
                "audio : retour vers le téléphone depuis les entrées {:?} ({} canaux)",
                a.return_from_input_channels, in_ch
            );
        }
        Ok(())
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

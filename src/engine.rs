//! Moteur vidéo/audio : pipeline GStreamer principal.
//!
//! ```text
//! intervideosrc(phone) ─ tee ─┬─► scène A (compositor) ─ tee ─┬─► program (compositor, alpha/zorder) ─ tee ─┬─► sortie HDMI (appsink)
//!                             │                              ├─► preview (input-selector) ─────────────┐   └─► tuile PROGRAM du multiview
//!                             └─► scène B ...               └─► tuile scène du multiview              └─► tuile PREVIEW du multiview
//! interaudiosrc(phone) ─ audioconvert(mix-matrix) ─► carte son (Wing)
//! carte son (Wing) ─ audioconvert(mix-matrix) ─► interaudiosink(return) ─► (pipeline WebRTC) ─► téléphone
//! ```

use crate::audio;
use crate::config::{parse_color, Config, Geometry, LayerConfig};
use crate::layout::{self, MultiviewLayout};
use anyhow::{Context, Result};
use gst::prelude::*;
use serde::Serialize;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

/// Canaux inter-pipelines (partagés avec le pipeline WebRTC).
pub const PHONE_VIDEO_CHANNEL: &str = "streame-phone-video";
pub const PHONE_AUDIO_CHANNEL: &str = "streame-phone-audio";
pub const RETURN_AUDIO_CHANNEL: &str = "streame-return-audio";

const NO_PHONE_TEXT: &str = "EN ATTENTE DU TÉLÉPHONE";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target {
    Program,
    Multiview,
}

/// Callback recevant les images de sortie (BGRx) pour affichage dans une fenêtre.
pub type FrameCallback = Arc<dyn Fn(Target, gst::Sample) + Send + Sync>;

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

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub scenes: Vec<SceneInfo>,
    pub program: String,
    pub preview: String,
    pub phone_connected: bool,
    pub phone_name: Option<String>,
    pub transition: String,
}

struct SceneHandle {
    id: String,
    name: String,
    program_pad: gst::Pad,
    preview_pad: gst::Pad,
}

struct State {
    program: usize,
    preview: usize,
    generation: u64,
    zorder: u32,
    phone: Option<String>,
}

pub struct Engine {
    cfg: Arc<Config>,
    pipeline: gst::Pipeline,
    scenes: Vec<SceneHandle>,
    preview_selector: gst::Element,
    /// `textoverlay` d'attente du téléphone (None si le plugin pango est absent).
    phone_text: Option<gst::Element>,
    program_out_caps: Option<gst::Element>,
    multiview_out_caps: Option<gst::Element>,
    layout: MultiviewLayout,
    state: Mutex<State>,
    events: broadcast::Sender<Event>,
}

fn make(factory: &str) -> Result<gst::Element> {
    gst::ElementFactory::make(factory).build().with_context(|| {
        format!("élément GStreamer « {factory} » indisponible (plugin manquant ?)")
    })
}

fn make_named(factory: &str, name: &str) -> Result<gst::Element> {
    gst::ElementFactory::make(factory)
        .name(name)
        .build()
        .with_context(|| {
            format!("élément GStreamer « {factory} » indisponible (plugin manquant ?)")
        })
}

fn video_caps(w: i32, h: i32, fps: i32, format: Option<&str>) -> gst::Caps {
    let mut b = gst::Caps::builder("video/x-raw")
        .field("width", w)
        .field("height", h)
        .field("framerate", gst::Fraction::new(fps, 1))
        .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
        // Toujours fixé : évite des échecs de négociation entre compositors (interlace-mode absent).
        .field("interlace-mode", "progressive");
    if let Some(f) = format {
        b = b.field("format", f);
    }
    b.build()
}

fn has_textoverlay() -> bool {
    static WARNED: std::sync::Once = std::sync::Once::new();
    let ok = gst::ElementFactory::find("textoverlay").is_some();
    if !ok {
        WARNED.call_once(|| {
            warn!("élément « textoverlay » absent (plugin pango) : pas de libellés à l'écran")
        });
    }
    ok
}

/// `textoverlay` configuré, ou `identity` si le plugin pango est absent.
fn text_overlay(text: &str, font: &str, valign: &str, halign: &str) -> Result<gst::Element> {
    if !has_textoverlay() {
        return make("identity");
    }
    Ok(gst::ElementFactory::make("textoverlay")
        .property("text", text)
        .property("font-desc", font)
        .property_from_str("valignment", valign)
        .property_from_str("halignment", halign)
        .property("shaded-background", true)
        .build()?)
}

/// Caps des images envoyées aux fenêtres (BGRx, taille de la fenêtre).
fn output_caps(w: i32, h: i32) -> gst::Caps {
    gst::Caps::builder("video/x-raw")
        .field("format", "BGRx")
        .field("width", w)
        .field("height", h)
        .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
        .field("interlace-mode", "progressive")
        .build()
}

fn capsfilter(caps: &gst::Caps) -> Result<gst::Element> {
    Ok(gst::ElementFactory::make("capsfilter")
        .property("caps", caps)
        .build()?)
}

/// File d'attente non bloquante pour les branches de tee.
fn leaky_queue() -> Result<gst::Element> {
    Ok(gst::ElementFactory::make("queue")
        .property("max-size-buffers", 3u32)
        .property("max-size-time", 0u64)
        .property("max-size-bytes", 0u32)
        .property_from_str("leaky", "downstream")
        .build()?)
}

fn link_many(els: &[&gst::Element]) -> Result<()> {
    for w in els.windows(2) {
        w[0].link(w[1])
            .with_context(|| format!("liaison {} → {}", w[0].name(), w[1].name()))?;
    }
    Ok(())
}

fn apply_geometry(pad: &gst::Pad, g: &Geometry, opacity: f64) {
    pad.set_property("xpos", g.x);
    pad.set_property("ypos", g.y);
    if let Some(w) = g.width {
        pad.set_property("width", w);
    }
    if let Some(h) = g.height {
        pad.set_property("height", h);
    }
    pad.set_property("alpha", opacity.clamp(0.0, 1.0));
}

impl Engine {
    /// `frames` : callback d'affichage (fenêtres natives). `None` = `autovideosink`
    /// (ou `fakesink` si `fake_output`, pour les tests sans écran).
    pub fn new(
        cfg: Arc<Config>,
        frames: Option<FrameCallback>,
        fake_output: bool,
    ) -> Result<Arc<Engine>> {
        let (events, _) = broadcast::channel(64);
        let pipeline = gst::Pipeline::with_name("streame");
        let v = &cfg.video;
        let full_caps = video_caps(v.width, v.height, v.fps, None);
        let bgra_caps = video_caps(v.width, v.height, v.fps, Some("BGRA"));
        let mixer_latency = cfg.video.mixer_latency_ms * gst::ClockTime::MSECOND.nseconds();

        let new_mixer = |name: &str| -> Result<gst::Element> {
            let m = gst::ElementFactory::make("compositor")
                .name(name)
                .property_from_str("background", "black")
                .property("latency", mixer_latency)
                .property("min-upstream-latency", mixer_latency)
                .build()?;
            Ok(m)
        };

        // ---- Source téléphone (vidéo) --------------------------------------------------------
        let phone_src = gst::ElementFactory::make("intervideosrc")
            .name("phone_video_src")
            .property("channel", PHONE_VIDEO_CHANNEL)
            .property("timeout", 500u64 * gst::ClockTime::MSECOND.nseconds())
            .build()?;
        let phone_convert = make("videoconvert")?;
        let phone_scale = gst::ElementFactory::make("videoscale")
            .property("add-borders", true)
            .build()?;
        let phone_caps = capsfilter(&full_caps)?;
        let phone_text = text_overlay(NO_PHONE_TEXT, "Sans Bold 36", "center", "center")?;
        let phone_tee = gst::ElementFactory::make("tee")
            .name("phone_tee")
            .property("allow-not-linked", true)
            .build()?;
        pipeline.add_many([
            &phone_src,
            &phone_convert,
            &phone_scale,
            &phone_caps,
            &phone_text,
            &phone_tee,
        ])?;
        link_many(&[
            &phone_src,
            &phone_convert,
            &phone_scale,
            &phone_caps,
            &phone_text,
            &phone_tee,
        ])?;

        // ---- Programme, preview, multiview --------------------------------------------------
        let program_mixer = new_mixer("program")?;
        let program_caps = capsfilter(&bgra_caps)?;
        let program_tee = make_named("tee", "program_tee")?;
        pipeline.add_many([&program_mixer, &program_caps, &program_tee])?;
        link_many(&[&program_mixer, &program_caps, &program_tee])?;

        let preview_selector = gst::ElementFactory::make("input-selector")
            .name("preview")
            .property_from_str("sync-mode", "clock")
            .build()?;
        pipeline.add(&preview_selector)?;

        let mv = &cfg.multiview;
        let layout = layout::compute(mv.width, mv.height, cfg.scenes.len(), mv.columns);
        let multiview_mixer = if mv.enabled {
            let m = new_mixer("multiview")?;
            let c = capsfilter(&video_caps(mv.width, mv.height, v.fps, Some("BGRA")))?;
            pipeline.add_many([&m, &c])?;
            m.link(&c)?;
            Some((m, c))
        } else {
            None
        };

        // Tuile PROGRAM et PREVIEW du multiview
        if let Some((mvm, _)) = &multiview_mixer {
            let font = format!("Sans Bold {}", (mv.height / 24).max(12));
            for (label, src_el) in [("PROGRAM", &program_tee), ("PREVIEW", &preview_selector)] {
                let q = leaky_queue()?;
                let t = text_overlay(label, &font, "top", "left")?;
                pipeline.add_many([&q, &t])?;
                src_el.link(&q)?;
                q.link(&t)?;
                let pad = mvm.request_pad_simple("sink_%u").context("pad multiview")?;
                let r = if label == "PROGRAM" {
                    layout.program
                } else {
                    layout.preview
                };
                apply_geometry(
                    &pad,
                    &Geometry {
                        x: r.x,
                        y: r.y,
                        width: Some(r.w),
                        height: Some(r.h),
                    },
                    1.0,
                );
                t.static_pad("src").unwrap().link(&pad)?;
            }
        }

        // ---- Scènes -------------------------------------------------------------------------
        let mut scenes = Vec::new();
        for (idx, sc) in cfg.scenes.iter().enumerate() {
            let mixer = new_mixer(&format!("scene_{}", sc.id))?;
            let caps = capsfilter(&bgra_caps)?;
            let tee = make_named("tee", &format!("scene_{}_tee", sc.id))?;
            pipeline.add_many([&mixer, &caps, &tee])?;
            link_many(&[&mixer, &caps, &tee])?;

            let mut layers = sc.layers.clone();
            if layers.is_empty() {
                layers.push(LayerConfig::Color {
                    color: "#000000".into(),
                    geometry: Geometry::default(),
                });
            }
            for (li, layer) in layers.iter().enumerate() {
                let pad = mixer
                    .request_pad_simple("sink_%u")
                    .context("pad de scène")?;
                pad.set_property("zorder", li as u32);
                if let Err(e) = Self::build_layer(&cfg, &pipeline, &phone_tee, layer, &pad) {
                    warn!("scène « {} », calque {} ignoré : {e:#}", sc.name, li + 1);
                    mixer.release_request_pad(&pad);
                }
            }

            // → programme
            let q = leaky_queue()?;
            pipeline.add(&q)?;
            tee.link(&q)?;
            let program_pad = program_mixer
                .request_pad_simple("sink_%u")
                .context("pad programme")?;
            program_pad.set_property("alpha", if idx == 0 { 1.0f64 } else { 0.0f64 });
            program_pad.set_property("zorder", idx as u32);
            q.static_pad("src").unwrap().link(&program_pad)?;

            // → preview
            let q = leaky_queue()?;
            pipeline.add(&q)?;
            tee.link(&q)?;
            let preview_pad = preview_selector
                .request_pad_simple("sink_%u")
                .context("pad preview")?;
            q.static_pad("src").unwrap().link(&preview_pad)?;

            // → multiview
            if let Some((mvm, _)) = &multiview_mixer {
                let q = leaky_queue()?;
                let t = text_overlay(
                    &sc.name,
                    &format!("Sans Bold {}", (v.height / 20).max(12)),
                    "bottom",
                    "left",
                )?;
                pipeline.add_many([&q, &t])?;
                tee.link(&q)?;
                q.link(&t)?;
                let pad = mvm.request_pad_simple("sink_%u").context("pad multiview")?;
                let r = layout.tiles[idx];
                apply_geometry(
                    &pad,
                    &Geometry {
                        x: r.x,
                        y: r.y,
                        width: Some(r.w),
                        height: Some(r.h),
                    },
                    1.0,
                );
                t.static_pad("src").unwrap().link(&pad)?;
            }

            scenes.push(SceneHandle {
                id: sc.id.clone(),
                name: sc.name.clone(),
                program_pad,
                preview_pad,
            });
        }
        preview_selector.set_property("active-pad", &scenes[0].preview_pad);

        // ---- Sorties vidéo ------------------------------------------------------------------
        let program_out_caps = Self::build_output(
            &pipeline,
            &program_tee,
            Target::Program,
            (v.width, v.height),
            frames.clone(),
            fake_output,
        )?;
        let multiview_out_caps = match &multiview_mixer {
            Some((_, c)) => Self::build_output(
                &pipeline,
                c,
                Target::Multiview,
                (mv.width, mv.height),
                frames.clone(),
                fake_output,
            )?,
            None => None,
        };

        // ---- Audio --------------------------------------------------------------------------
        if cfg.audio.enabled {
            if let Err(e) = Self::build_audio(&cfg, &pipeline) {
                error!("audio désactivé : {e:#}");
            }
        }

        let engine = Arc::new(Engine {
            cfg: cfg.clone(),
            pipeline,
            scenes,
            preview_selector,
            phone_text: if has_textoverlay() {
                Some(phone_text)
            } else {
                None
            },
            program_out_caps,
            multiview_out_caps,
            layout,
            state: Mutex::new(State {
                program: 0,
                preview: 0,
                generation: 0,
                zorder: cfg.scenes.len() as u32,
                phone: None,
            }),
            events,
        });
        engine.spawn_bus_thread();
        Ok(engine)
    }

    /// Construit la branche de sortie (fenêtre via appsink, ou autovideosink).
    fn build_output(
        pipeline: &gst::Pipeline,
        src: &gst::Element,
        target: Target,
        size: (i32, i32),
        frames: Option<FrameCallback>,
        fake_output: bool,
    ) -> Result<Option<gst::Element>> {
        let q = leaky_queue()?;
        let convert = make("videoconvert")?;
        let scale = gst::ElementFactory::make("videoscale")
            .property("add-borders", true)
            .build()?;
        pipeline.add_many([&q, &convert, &scale])?;
        src.link(&q)?;
        link_many(&[&q, &convert, &scale])?;
        match frames {
            Some(cb) => {
                let caps = output_caps(size.0, size.1);
                let cf = capsfilter(&caps)?;
                let sink = gst_app::AppSink::builder()
                    .name(match target {
                        Target::Program => "program_out",
                        Target::Multiview => "multiview_out",
                    })
                    .caps(
                        &gst::Caps::builder("video/x-raw")
                            .field("format", "BGRx")
                            .build(),
                    )
                    .drop(true)
                    .max_buffers(1)
                    .sync(true)
                    .build();
                sink.set_callbacks(
                    gst_app::AppSinkCallbacks::builder()
                        .new_sample(move |s| {
                            let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                            cb(target, sample);
                            Ok(gst::FlowSuccess::Ok)
                        })
                        .build(),
                );
                pipeline.add_many([&cf, sink.upcast_ref()])?;
                scale.link(&cf)?;
                cf.link(&sink)?;
                Ok(Some(cf))
            }
            None => {
                let factory = if fake_output {
                    "fakesink"
                } else {
                    "autovideosink"
                };
                let sink = gst::ElementFactory::make(factory)
                    .property("sync", true)
                    .build()?;
                pipeline.add(&sink)?;
                scale.link(&sink)?;
                Ok(None)
            }
        }
    }

    fn build_layer(
        cfg: &Config,
        pipeline: &gst::Pipeline,
        phone_tee: &gst::Element,
        layer: &LayerConfig,
        pad: &gst::Pad,
    ) -> Result<()> {
        let v = &cfg.video;
        match layer {
            LayerConfig::Phone { geometry, opacity } => {
                let q = leaky_queue()?;
                pipeline.add(&q)?;
                phone_tee.link(&q)?;
                q.static_pad("src").unwrap().link(pad)?;
                apply_geometry(pad, geometry, *opacity);
            }
            LayerConfig::Color { color, geometry } => {
                let (r, g, b, a) =
                    parse_color(color).with_context(|| format!("couleur invalide : {color}"))?;
                let argb = ((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | b as u32;
                let src = gst::ElementFactory::make("videotestsrc")
                    .property("is-live", true)
                    .property_from_str("pattern", "solid-color")
                    .property("foreground-color", argb)
                    .build()?;
                let w = geometry.width.unwrap_or(v.width);
                let h = geometry.height.unwrap_or(v.height);
                let cf = capsfilter(&video_caps(w, h, v.fps, Some("BGRA")))?;
                pipeline.add_many([&src, &cf])?;
                src.link(&cf)?;
                cf.static_pad("src").unwrap().link(pad)?;
                apply_geometry(pad, geometry, 1.0);
            }
            LayerConfig::Text {
                text,
                font,
                color,
                geometry,
            } => {
                anyhow::ensure!(
                    has_textoverlay(),
                    "calque texte impossible sans le plugin pango"
                );
                let (r, g, b, a) =
                    parse_color(color).with_context(|| format!("couleur invalide : {color}"))?;
                let argb = ((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | b as u32;
                let src = gst::ElementFactory::make("videotestsrc")
                    .property("is-live", true)
                    .property_from_str("pattern", "solid-color")
                    .property("foreground-color", 0u32)
                    .build()?;
                let w = geometry.width.unwrap_or(v.width);
                let h = geometry.height.unwrap_or(v.height);
                let cf = capsfilter(&video_caps(w, h, v.fps, Some("BGRA")))?;
                let t = gst::ElementFactory::make("textoverlay")
                    .property("text", text)
                    .property("font-desc", font)
                    .property("color", argb)
                    .property_from_str("valignment", "center")
                    .property_from_str("halignment", "center")
                    .build()?;
                pipeline.add_many([&src, &cf, &t])?;
                link_many(&[&src, &cf, &t])?;
                t.static_pad("src").unwrap().link(pad)?;
                apply_geometry(pad, geometry, 1.0);
            }
            LayerConfig::Image {
                path,
                geometry,
                opacity,
            } => {
                let file = cfg.resolve(path);
                anyhow::ensure!(file.is_file(), "image introuvable : {}", file.display());
                let src = gst::ElementFactory::make("filesrc")
                    .property("location", file.to_str().unwrap_or(path))
                    .build()?;
                let dec = make("decodebin")?;
                let freeze = gst::ElementFactory::make("imagefreeze")
                    .property("is-live", true)
                    .build()?;
                let convert = make("videoconvert")?;
                let cf = capsfilter(
                    &gst::Caps::builder("video/x-raw")
                        .field("format", "BGRA")
                        .field("framerate", gst::Fraction::new(v.fps, 1))
                        .field("interlace-mode", "progressive")
                        .build(),
                )?;
                pipeline.add_many([&src, &dec, &freeze, &convert, &cf])?;
                src.link(&dec)?;
                link_many(&[&freeze, &convert, &cf])?;
                cf.static_pad("src").unwrap().link(pad)?;
                let freeze_sink = freeze.static_pad("sink").unwrap();
                dec.connect_pad_added(move |_, dpad| {
                    if let Err(e) = dpad.link(&freeze_sink) {
                        warn!("image : liaison decodebin impossible : {e:?}");
                    }
                });
                apply_geometry(pad, geometry, *opacity);
            }
            LayerConfig::Video {
                path,
                looped,
                geometry,
                opacity,
            } => {
                let file = cfg.resolve(path);
                anyhow::ensure!(file.is_file(), "vidéo introuvable : {}", file.display());
                let file = std::fs::canonicalize(&file).unwrap_or(file);
                let uri =
                    gst::glib::filename_to_uri(&file, None).context("uri du fichier vidéo")?;
                let dec = gst::ElementFactory::make("uridecodebin")
                    .property("uri", uri.as_str())
                    .build()?;
                let q = make("queue")?;
                let convert = make("videoconvert")?;
                let scale = make("videoscale")?;
                pipeline.add_many([&dec, &q, &convert, &scale])?;
                link_many(&[&q, &convert, &scale])?;
                scale.static_pad("src").unwrap().link(pad)?;
                if tracing::enabled!(tracing::Level::TRACE) {
                    // Diagnostic : temps des images du fichier vs temps du pipeline.
                    let pipe = pipeline.downgrade();
                    let count = std::sync::atomic::AtomicU64::new(0);
                    scale.static_pad("src").unwrap().add_probe(
                        gst::PadProbeType::BUFFER,
                        move |p, info| {
                            if !count
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                                .is_multiple_of(30)
                            {
                                return gst::PadProbeReturn::Ok;
                            }
                            if let (Some(gst::PadProbeData::Buffer(b)), Some(pipe)) =
                                (&info.data, pipe.upgrade())
                            {
                                let seg = p
                                    .sticky_event::<gst::event::Segment>(0)
                                    .map(|e| e.segment().clone());
                                let rt = seg
                                    .and_then(|s| s.downcast::<gst::ClockTime>().ok())
                                    .and_then(|s| b.pts().and_then(|pts| s.to_running_time(pts)));
                                let prt = pipe.base_time().and_then(|bt| {
                                    pipe.clock().map(|c| c.time().saturating_sub(bt))
                                });
                                tracing::trace!(
                                    "fichier vidéo : image à {:?} (+offset {} ns), pipeline à {:?}",
                                    rt,
                                    p.offset(),
                                    prt
                                );
                            }
                            gst::PadProbeReturn::Ok
                        },
                    );
                }
                let q_sink = q.static_pad("sink").unwrap();
                let pipe = pipeline.downgrade();
                let looped = *looped;
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
                            // Boucle sans coupure : seek « segment » initial (flushant), puis à chaque
                            // événement segment-done du démultiplexeur, un seek segment non flushant
                            // (le temps de lecture reste continu, pas d'à-coup dans le mélangeur).
                            // L'événement est intercepté ici (par branche) plutôt que le message du bus,
                            // que le pipeline agrège pour toutes les vidéos en boucle.
                            let dec = dec.clone();
                            let dpad = dpad.clone();
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
                            thread::spawn(move || {
                                // Après un seek flushant sur cette branche seule, le temps de lecture repart
                                // à zéro : on compense avec un décalage de pad égal au temps courant du pipeline.
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
                apply_geometry(pad, geometry, *opacity);
            }
        }
        Ok(())
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
                        MessageView::Eos(_) => warn!("GStreamer : fin de flux inattendue"),
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
            "pipeline démarré ({}x{}@{})",
            self.cfg.video.width, self.cfg.video.height, self.cfg.video.fps
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

    pub fn layout(&self) -> &MultiviewLayout {
        &self.layout
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

    pub fn snapshot(&self) -> Snapshot {
        let st = self.state.lock().unwrap();
        Snapshot {
            scenes: self.scenes(),
            program: self.scenes[st.program].id.clone(),
            preview: self.scenes[st.preview].id.clone(),
            phone_connected: st.phone.is_some(),
            phone_name: st.phone.clone(),
            transition: self.cfg.transition.kind.clone(),
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
        self.preview_selector
            .set_property("active-pad", &self.scenes[idx].preview_pad);
        let _ = self.events.send(Event::Preview {
            scene: self.scenes[idx].id.clone(),
        });
    }

    /// Preview → programme (avec la transition configurée).
    pub fn take(self: &Arc<Self>) {
        let (prog, prev) = {
            let st = self.state.lock().unwrap();
            (st.program, st.preview)
        };
        self.switch_program(prev, true);
        // Le programme précédent devient la preview (comme sur une régie).
        self.set_preview(prog);
    }

    /// Bascule immédiate.
    pub fn cut(self: &Arc<Self>, idx: usize) {
        self.switch_program(idx, false);
    }

    /// Bascule avec la transition configurée.
    pub fn transition_to(self: &Arc<Self>, idx: usize) {
        self.switch_program(idx, true);
    }

    fn switch_program(self: &Arc<Self>, idx: usize, animated: bool) {
        if idx >= self.scenes.len() {
            return;
        }
        let (generation, zorder) = {
            let mut st = self.state.lock().unwrap();
            if st.program == idx {
                return;
            }
            st.generation += 1;
            st.zorder += 1;
            st.program = idx;
            (st.generation, st.zorder)
        };
        let pad = self.scenes[idx].program_pad.clone();
        pad.set_property("zorder", zorder);
        let _ = self.events.send(Event::Program {
            scene: self.scenes[idx].id.clone(),
        });

        let duration = self.cfg.transition.duration_ms;
        let fade =
            animated && self.cfg.transition.kind.eq_ignore_ascii_case("fade") && duration > 0;
        if !fade {
            pad.set_property("alpha", 1.0f64);
            self.finish_switch(idx);
            return;
        }
        let engine = self.clone();
        thread::spawn(move || {
            let step = Duration::from_millis(16);
            let steps = (duration / 16).max(1);
            let start = pad.property::<f64>("alpha");
            for s in 1..=steps {
                if engine.state.lock().unwrap().generation != generation {
                    return;
                }
                let t = s as f64 / steps as f64;
                pad.set_property("alpha", start + (1.0 - start) * t);
                thread::sleep(step);
            }
            engine.finish_switch(idx);
        });
    }

    fn finish_switch(&self, idx: usize) {
        for (i, s) in self.scenes.iter().enumerate() {
            if i != idx {
                s.program_pad.set_property("alpha", 0.0f64);
            }
        }
    }

    /// Notifie la connexion/déconnexion du téléphone.
    pub fn set_phone(&self, name: Option<String>) {
        {
            let mut st = self.state.lock().unwrap();
            st.phone = name.clone();
        }
        if let Some(t) = &self.phone_text {
            t.set_property("text", if name.is_some() { "" } else { NO_PHONE_TEXT });
        }
        let _ = self.events.send(Event::Phone {
            connected: name.is_some(),
            name,
        });
    }

    /// Adapte la taille des images envoyées à une fenêtre.
    pub fn set_output_size(&self, target: Target, width: u32, height: u32) {
        let cf = match target {
            Target::Program => &self.program_out_caps,
            Target::Multiview => &self.multiview_out_caps,
        };
        if let (Some(cf), true) = (cf, width > 0 && height > 0) {
            cf.set_property("caps", output_caps(width as i32, height as i32));
        }
    }
}

/// Vérifie la présence des éléments GStreamer nécessaires.
pub fn check_elements() -> Vec<&'static str> {
    const REQUIRED: &[&str] = &[
        "compositor",
        "videoconvert",
        "videoscale",
        "videotestsrc",
        "capsfilter",
        "tee",
        "queue",
        "input-selector",
        "textoverlay",
        "intervideosrc",
        "intervideosink",
        "interaudiosrc",
        "interaudiosink",
        "imagefreeze",
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

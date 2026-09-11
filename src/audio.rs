//! Périphériques audio (Behringer Wing ou autre carte multicanal) et matrices de routage.

use anyhow::{anyhow, Result};
use gst::prelude::*;
use tracing::{info, warn};

#[derive(Debug, Clone)]
pub struct AudioDevice {
    pub name: String,
    pub class: String,
    pub max_channels: Option<i32>,
    pub device: gst::Device,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Sink,
    Source,
}

impl Direction {
    fn class(self) -> &'static str {
        match self {
            Direction::Sink => "Audio/Sink",
            Direction::Source => "Audio/Source",
        }
    }
}

/// Liste les périphériques audio connus de GStreamer.
pub fn list_devices() -> Vec<AudioDevice> {
    let monitor = gst::DeviceMonitor::new();
    monitor.add_filter(Some("Audio/Sink"), None);
    monitor.add_filter(Some("Audio/Source"), None);
    if let Err(e) = monitor.start() {
        warn!("impossible de démarrer l'énumération audio : {e}");
        return Vec::new();
    }
    let devices = monitor
        .devices()
        .iter()
        .map(|d| AudioDevice {
            name: d.display_name().to_string(),
            class: d.device_class().to_string(),
            max_channels: d.caps().and_then(|c| max_channels_from_caps(&c)),
            device: d.clone(),
        })
        .collect();
    monitor.stop();
    devices
}

fn max_channels_from_caps(caps: &gst::Caps) -> Option<i32> {
    let mut max = None;
    for s in caps.iter() {
        let v = if let Ok(n) = s.get::<i32>("channels") {
            Some(n)
        } else if let Ok(r) = s.get::<gst::IntRange<i32>>("channels") {
            Some(r.max())
        } else {
            None
        };
        if let Some(n) = v {
            max = Some(max.map_or(n, |m: i32| m.max(n)));
        }
    }
    max
}

pub fn find_device(devices: &[AudioDevice], dir: Direction, pattern: &str) -> Option<AudioDevice> {
    let p = pattern.to_lowercase();
    devices
        .iter()
        .find(|d| d.class.contains(dir.class()) && d.name.to_lowercase().contains(&p))
        .cloned()
}

/// Crée l'élément sink/source correspondant à la sélection ("default", "none", ou un nom).
/// Retourne l'élément et le nombre de canaux (0 si inconnu).
pub fn make_device_element(
    selection: &str,
    dir: Direction,
    configured_channels: i32,
) -> Result<Option<(gst::Element, i32)>> {
    let sel = selection.trim();
    if sel.eq_ignore_ascii_case("none") || sel.is_empty() {
        return Ok(None);
    }
    if sel.eq_ignore_ascii_case("default") {
        let factory = match dir {
            Direction::Sink => "autoaudiosink",
            Direction::Source => "autoaudiosrc",
        };
        let el = gst::ElementFactory::make(factory).build()?;
        let ch = if configured_channels > 0 {
            configured_channels
        } else {
            2
        };
        return Ok(Some((el, ch)));
    }
    let devices = list_devices();
    let dev = find_device(&devices, dir, sel).ok_or_else(|| {
        let names: Vec<_> = devices
            .iter()
            .filter(|d| d.class.contains(dir.class()))
            .map(|d| d.name.as_str())
            .collect();
        anyhow!(
            "périphérique audio « {sel} » ({}) introuvable. Disponibles : {}",
            dir.class(),
            names.join(", ")
        )
    })?;
    let el = dev.device.create_element(None)?;
    let ch = if configured_channels > 0 {
        configured_channels
    } else {
        dev.max_channels.unwrap_or(2)
    };
    info!("audio {:?} : « {} » ({} canaux)", dir, dev.name, ch);
    Ok(Some((el, ch)))
}

/// Matrice (out x in) envoyant chaque canal d'entrée vers le canal de sortie
/// `targets[i]` (1-based). Si moins de cibles que d'entrées, les entrées sont
/// sommées sur la même cible avec un gain réduit.
pub fn route_matrix(in_channels: usize, out_channels: usize, targets: &[usize]) -> Vec<Vec<f32>> {
    let mut m = vec![vec![0.0f32; in_channels]; out_channels];
    if targets.is_empty() || in_channels == 0 || out_channels == 0 {
        return m;
    }
    let gain = if targets.len() < in_channels {
        1.0 / in_channels as f32
    } else {
        1.0
    };
    for i in 0..in_channels {
        let t = targets[i % targets.len()];
        if t >= 1 && t <= out_channels {
            m[t - 1][i] += gain;
        } else {
            warn!("canal de sortie {t} hors limites (1..={out_channels})");
        }
    }
    m
}

/// Matrice (out x in) sélectionnant les canaux d'entrée `sources` (1-based)
/// vers `out_channels` sorties (stéréo en général).
pub fn select_matrix(in_channels: usize, out_channels: usize, sources: &[usize]) -> Vec<Vec<f32>> {
    let mut m = vec![vec![0.0f32; in_channels]; out_channels];
    if sources.is_empty() || in_channels == 0 {
        return m;
    }
    for (j, row) in m.iter_mut().enumerate() {
        let s = sources[j % sources.len()];
        if s >= 1 && s <= in_channels {
            row[s - 1] = 1.0;
        } else {
            warn!("canal d'entrée {s} hors limites (1..={in_channels})");
        }
    }
    m
}

/// Convertit une matrice en valeur GStreamer pour `audioconvert::mix-matrix`.
pub fn to_gst_matrix(m: &[Vec<f32>]) -> gst::Array {
    gst::Array::new(m.iter().map(|row| gst::Array::new(row.iter().copied())))
}

/// Caps audio brutes pour N canaux. Le masque de canaux est laissé libre :
/// avec `mix-matrix`, audioconvert ignore les positions et prend celles du périphérique.
pub fn raw_caps(rate: i32, channels: i32) -> gst::Caps {
    let mut b = gst::Caps::builder("audio/x-raw")
        .field("rate", rate)
        .field("channels", channels);
    // Au-delà de 2 canaux, on adresse directement les canaux physiques de la carte
    // (non positionnés) : sans masque, la négociation multicanal échoue avec osxaudiosink.
    if channels > 2 {
        b = b.field("channel-mask", gst::Bitmask::new(0));
    }
    b.build()
}

/// Sortie audio multicanal via CoreAudio (cpal), car `osxaudiosink` de GStreamer se limite
/// à 2 canaux sur cette carte. Le graphe GStreamer produit un flux entrelacé F32 à N canaux
/// (mélange habillage + stream déjà routés) qu'un `appsink` pousse dans un tampon, lu par le
/// flux CoreAudio.
#[cfg(target_os = "macos")]
pub mod cpal_out {
    use anyhow::{anyhow, Result};
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use tracing::{error, info};

    /// Tampon partagé entre l'appsink (producteur) et le flux CoreAudio (consommateur).
    pub type Ring = Arc<Mutex<VecDeque<f32>>>;

    /// Flux de sortie CoreAudio maintenu en vie (le flux cpal n'est pas `Send` : il vit dans
    /// son propre thread).
    pub struct Output {
        pub ring: Ring,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Drop for Output {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(t) = self.thread.take() {
                t.thread().unpark();
                let _ = t.join();
            }
        }
    }

    fn find_device(name: &str) -> Result<cpal::Device> {
        let host = cpal::default_host();
        let low = name.to_lowercase();
        host.output_devices()?
            .find(|d| {
                d.name()
                    .map(|n| n.to_lowercase().contains(&low))
                    .unwrap_or(false)
            })
            .ok_or_else(|| anyhow!("périphérique de sortie CoreAudio « {name} » introuvable"))
    }

    /// Meilleure configuration (nombre de canaux maximal, 48 kHz si possible).
    pub fn best_config(name: &str) -> Result<(u16, u32)> {
        let dev = find_device(name)?;
        let def = dev.default_output_config()?;
        let mut best = (def.channels(), def.sample_rate().0);
        if let Ok(cfgs) = dev.supported_output_configs() {
            for c in cfgs {
                if c.channels() >= best.0 {
                    let rate = if c.min_sample_rate().0 <= 48000 && 48000 <= c.max_sample_rate().0 {
                        48000
                    } else {
                        c.max_sample_rate().0
                    };
                    best = (c.channels(), rate);
                }
            }
        }
        Ok(best)
    }

    /// Ouvre le flux CoreAudio ; il lit `ring` (F32 entrelacé) et sort silence si sous-alimenté.
    pub fn start(name: &str, channels: u16, sample_rate: u32) -> Result<Output> {
        let dev = find_device(name)?;
        let config = cpal::StreamConfig {
            channels,
            sample_rate: cpal::SampleRate(sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };
        let ring: Ring = Arc::new(Mutex::new(VecDeque::with_capacity(
            sample_rate as usize * channels as usize / 4,
        )));
        let stop = Arc::new(AtomicBool::new(false));
        let (ring_cb, stop_thread, name_owned) = (ring.clone(), stop.clone(), name.to_string());
        let thread = std::thread::Builder::new()
            .name("cpal-out".into())
            .spawn(move || {
                let stream = match dev.build_output_stream(
                    &config,
                    move |out: &mut [f32], _| match ring_cb.try_lock() {
                        Ok(mut r) => {
                            for s in out.iter_mut() {
                                *s = r.pop_front().unwrap_or(0.0);
                            }
                        }
                        Err(_) => out.iter_mut().for_each(|s| *s = 0.0),
                    },
                    |e| error!("flux CoreAudio : {e}"),
                    None,
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        error!("création du flux CoreAudio « {name_owned} » : {e}");
                        return;
                    }
                };
                if let Err(e) = stream.play() {
                    error!("démarrage du flux CoreAudio : {e}");
                    return;
                }
                info!("sortie CoreAudio « {name_owned} » : {channels} canaux @ {sample_rate} Hz");
                while !stop_thread.load(Ordering::Relaxed) {
                    std::thread::park_timeout(std::time::Duration::from_millis(250));
                }
            })?;
        Ok(Output {
            ring,
            stop,
            thread: Some(thread),
        })
    }

    /// Pousse des échantillons F32 entrelacés, en bornant la latence (jette le plus ancien).
    pub fn push(ring: &Ring, samples: &[f32], max_samples: usize) {
        if let Ok(mut r) = ring.lock() {
            if r.len() + samples.len() > max_samples {
                let len = r.len();
                let excess = (len + samples.len()) - max_samples;
                r.drain(0..excess.min(len));
            }
            r.extend(samples.iter().copied());
        }
    }
}

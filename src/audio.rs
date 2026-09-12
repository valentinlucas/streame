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
    use ringbuf::traits::{Consumer, Observer, Producer, Split};
    use ringbuf::HeapRb;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tracing::{error, info, warn};

    use std::sync::atomic::AtomicU32;

    type Prod = ringbuf::HeapProd<f32>;

    /// Poignée d'écriture (côté appsink GStreamer). Le consommateur vit dans le thread temps
    /// réel CoreAudio et lit sans verrou.
    #[derive(Clone)]
    pub struct Pusher {
        prod: Arc<Mutex<Prod>>,
        /// Cible de remplissage (échantillons entrelacés) = latence visée.
        target: usize,
        capacity: usize,
        channels: usize,
    }

    /// VU-mètre d'une source (2 canaux), mis à jour dans le callback temps réel (sans verrou).
    #[derive(Default)]
    pub struct SourceMeter {
        rms: [AtomicU32; 2],
        peak: [AtomicU32; 2],
    }

    impl SourceMeter {
        /// Calcule RMS/crête par canal sur un bloc stéréo entrelacé et les stocke.
        fn store_stereo(&self, buf: &[f32]) {
            for ch in 0..2 {
                let mut sum = 0f64;
                let mut peak = 0f32;
                let mut n = 0u32;
                let mut i = ch;
                while i < buf.len() {
                    let x = buf[i];
                    sum += (x as f64) * (x as f64);
                    peak = peak.max(x.abs());
                    n += 1;
                    i += 2;
                }
                let rms = if n > 0 {
                    (sum / n as f64).sqrt() as f32
                } else {
                    0.0
                };
                self.rms[ch].store(rms.to_bits(), Ordering::Relaxed);
                self.peak[ch].store(peak.to_bits(), Ordering::Relaxed);
            }
        }

        /// (rms_dBFS, peak_dBFS) par canal.
        pub fn read_db(&self) -> (Vec<f32>, Vec<f32>) {
            let db = |bits: &AtomicU32| {
                let x = f32::from_bits(bits.load(Ordering::Relaxed));
                if x <= 1e-6 {
                    -100.0
                } else {
                    20.0 * x.log10()
                }
            };
            (
                vec![db(&self.rms[0]), db(&self.rms[1])],
                vec![db(&self.peak[0]), db(&self.peak[1])],
            )
        }
    }

    /// VU-mètres partagés (calculés dans les callbacks CoreAudio, lus par l'API/le multiview).
    #[derive(Default)]
    pub struct Meters {
        pub stream: SourceMeter,
        pub branding: SourceMeter,
        pub ret: SourceMeter,
    }

    /// Objet à garder en vie : maintient le thread et le flux CoreAudio.
    pub struct Output {
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
        if name.trim().eq_ignore_ascii_case("default") || name.trim().is_empty() {
            return host
                .default_output_device()
                .ok_or_else(|| anyhow!("aucune sortie CoreAudio par défaut"));
        }
        let low = name.to_lowercase();
        if let Some(d) = host.output_devices()?.find(|d| {
            d.name()
                .map(|n| n.to_lowercase().contains(&low))
                .unwrap_or(false)
        }) {
            return Ok(d);
        }
        // Périphérique nommé absent (ex. Wing débranchée) : repli sur la sortie par défaut
        // plutôt que de désactiver tout l'audio — les VU-mètres et l'habillage continuent.
        warn!("sortie CoreAudio « {name} » introuvable : repli sur la sortie par défaut");
        host.default_output_device().ok_or_else(|| {
            anyhow!("périphérique de sortie CoreAudio « {name} » introuvable et aucune sortie par défaut")
        })
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

    /// Ouvre le flux CoreAudio de SORTIE et fait le mixage/routage/mètres directement dans le
    /// callback temps réel, à l'horloge exacte de la carte. Deux sources stéréo (stream du
    /// téléphone et habillage) sont fournies par GStreamer via les `Pusher` renvoyés ; chacune
    /// est placée sur ses canaux (`stream_ch`/`branding_ch`, 0-based, modifiables à chaud) et
    /// sommée dans les N canaux de la carte.
    #[allow(clippy::type_complexity)]
    pub fn start_output_mixed(
        name: &str,
        out_channels: u16,
        sample_rate: u32,
        stream_ch: Arc<[AtomicUsize; 2]>,
        branding_ch: Arc<[AtomicUsize; 2]>,
        meters: Arc<Meters>,
    ) -> Result<(Output, Pusher, Pusher)> {
        let dev = find_device(name)?;
        let config = cpal::StreamConfig {
            channels: out_channels,
            sample_rate: cpal::SampleRate(sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };
        let n = out_channels as usize;
        let cap = (sample_rate as usize * 2 * 400 / 1000).max(4); // stéréo, ~400 ms
        let (phone_prod, mut phone_cons) = HeapRb::<f32>::new(cap).split();
        let (brand_prod, mut brand_cons) = HeapRb::<f32>::new(cap).split();
        // Scratch réutilisé (pas d'allocation dans le callback temps réel, sauf dépassement rare).
        let mut pbuf = vec![0f32; 8192 * 2];
        let mut bbuf = vec![0f32; 8192 * 2];
        let meters_cb = meters.clone();
        let (sc, bc) = (stream_ch, branding_ch);
        // Pré-tampon : on attend d'avoir accumulé ~90 ms avant de jouer une source, sinon la
        // moindre gigue vide l'anneau et le callback comble avec du silence → son haché. En cas
        // de sous-alimentation on repasse en pré-tampon (une coupure nette plutôt qu'un hachis).
        let prime = (sample_rate as usize * 2 * 90 / 1000).max(2);
        let mut phone_primed = false;
        let mut brand_primed = false;

        let stop = Arc::new(AtomicBool::new(false));
        let (stop_thread, name_owned) = (stop.clone(), name.to_string());
        let thread = std::thread::Builder::new()
            .name("cpal-out".into())
            .spawn(move || {
                let stream = match dev.build_output_stream(
                    &config,
                    move |out: &mut [f32], _| {
                        let frames = if n > 0 { out.len() / n } else { 0 };
                        let need = frames * 2;
                        if need > pbuf.len() {
                            pbuf.resize(need, 0.0);
                            bbuf.resize(need, 0.0);
                        }
                        // Source téléphone : lecture pré-tamponnée.
                        if !phone_primed && phone_cons.occupied_len() >= prime {
                            phone_primed = true;
                        }
                        let pn = if phone_primed {
                            phone_cons.pop_slice(&mut pbuf[..need])
                        } else {
                            0
                        };
                        pbuf[pn..need].iter_mut().for_each(|s| *s = 0.0);
                        if phone_primed && pn < need {
                            phone_primed = false; // sous-alimentation → on re-tamponne
                        }
                        // Source habillage : idem.
                        if !brand_primed && brand_cons.occupied_len() >= prime {
                            brand_primed = true;
                        }
                        let bn = if brand_primed {
                            brand_cons.pop_slice(&mut bbuf[..need])
                        } else {
                            0
                        };
                        bbuf[bn..need].iter_mut().for_each(|s| *s = 0.0);
                        if brand_primed && bn < need {
                            brand_primed = false;
                        }
                        meters_cb.stream.store_stereo(&pbuf[..need]);
                        meters_cb.branding.store_stereo(&bbuf[..need]);
                        let (s0, s1) = (sc[0].load(Ordering::Relaxed), sc[1].load(Ordering::Relaxed));
                        let (b0, b1) = (bc[0].load(Ordering::Relaxed), bc[1].load(Ordering::Relaxed));
                        out.iter_mut().for_each(|s| *s = 0.0);
                        for f in 0..frames {
                            let ob = f * n;
                            let (pl, pr) = (pbuf[f * 2], pbuf[f * 2 + 1]);
                            let (bl, br) = (bbuf[f * 2], bbuf[f * 2 + 1]);
                            if s0 < n {
                                out[ob + s0] += pl;
                            }
                            if s1 < n {
                                out[ob + s1] += pr;
                            }
                            if b0 < n {
                                out[ob + b0] += bl;
                            }
                            if b1 < n {
                                out[ob + b1] += br;
                            }
                        }
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
                info!("sortie CoreAudio « {name_owned} » : {out_channels} canaux @ {sample_rate} Hz (mixage cpal)");
                while !stop_thread.load(Ordering::Relaxed) {
                    std::thread::park_timeout(std::time::Duration::from_millis(250));
                }
            })?;

        let mk = |prod| Pusher {
            prod: Arc::new(Mutex::new(prod)),
            // Rétention visée quand on écrête (anneau presque plein, horloge carte plus lente) :
            // au-dessus du pré-tampon (90 ms) pour ne pas re-déclencher de sous-alimentation.
            target: (sample_rate as usize * 2 * 150 / 1000).max(2),
            capacity: cap,
            channels: 2,
        };
        Ok((
            Output {
                stop,
                thread: Some(thread),
            },
            mk(phone_prod),
            mk(brand_prod),
        ))
    }

    impl Pusher {
        /// Pousse des échantillons F32 entrelacés. Anti-dérive : si le tampon dépasse largement
        /// la cible (l'horloge de la carte est un peu plus lente), on saute des trames récentes
        /// pour ne pas laisser la latence grandir.
        pub fn push(&self, samples: &[f32]) {
            let Ok(mut prod) = self.prod.lock() else {
                return;
            };
            let occupied = prod.occupied_len();
            // Plafond dur : au-delà de la capacité, on jette (évite le blocage).
            if occupied >= self.capacity.saturating_sub(samples.len()) {
                // Trop en retard : on saute presque tout sauf la cible, en gardant l'alignement
                // sur les trames (multiples du nombre de canaux).
                let keep = self.target - (self.target % self.channels.max(1));
                let skip = samples.len().saturating_sub(keep.min(samples.len()));
                let start = skip - (skip % self.channels.max(1));
                let _ = prod.push_slice(&samples[start..]);
                warn!("sortie CoreAudio : tampon plein, trames sautées (dérive d'horloge)");
                return;
            }
            let _ = prod.push_slice(samples);
        }
    }

    // ------------------------------------------------------------------------------------------
    // ENTRÉE CoreAudio (retour de la carte vers le téléphone)
    // ------------------------------------------------------------------------------------------

    type Cons = ringbuf::HeapCons<f32>;

    /// Lecteur du retour (côté appsrc GStreamer). Le producteur est le thread temps réel
    /// d'entrée CoreAudio, sans verrou ; ici on lit sous verrou (thread GStreamer, non temps réel).
    #[derive(Clone)]
    pub struct Reader {
        cons: Arc<Mutex<Cons>>,
    }

    impl Reader {
        /// Remplit `out` (F32 stéréo entrelacé) avec le retour disponible ; renvoie le nombre
        /// d'échantillons fournis (le reste est à compléter en silence par l'appelant).
        pub fn pull(&self, out: &mut [f32]) -> usize {
            match self.cons.lock() {
                Ok(mut c) => c.pop_slice(out),
                Err(_) => 0,
            }
        }
    }

    fn find_input_device(name: &str) -> Result<cpal::Device> {
        let host = cpal::default_host();
        if name.trim().eq_ignore_ascii_case("default") || name.trim().is_empty() {
            return host
                .default_input_device()
                .ok_or_else(|| anyhow!("aucune entrée CoreAudio par défaut"));
        }
        let low = name.to_lowercase();
        if let Some(d) = host.input_devices()?.find(|d| {
            d.name()
                .map(|n| n.to_lowercase().contains(&low))
                .unwrap_or(false)
        }) {
            return Ok(d);
        }
        warn!("entrée CoreAudio « {name} » introuvable : repli sur l'entrée par défaut");
        host.default_input_device().ok_or_else(|| {
            anyhow!("périphérique d'entrée CoreAudio « {name} » introuvable et aucune entrée par défaut")
        })
    }

    /// Meilleure configuration d'entrée (canaux max, 48 kHz si possible).
    pub fn best_input_config(name: &str) -> Result<(u16, u32)> {
        let dev = find_input_device(name)?;
        let def = dev.default_input_config()?;
        let mut best = (def.channels(), def.sample_rate().0);
        if let Ok(cfgs) = dev.supported_input_configs() {
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

    /// Ouvre le flux d'ENTRÉE CoreAudio. Le callback temps réel sélectionne les 2 canaux de
    /// retour (indices 0-based dans `sel`, modifiables à chaud) et pousse du stéréo dans le tampon.
    pub fn start_input(
        name: &str,
        in_channels: u16,
        sample_rate: u32,
        sel: Arc<[AtomicUsize; 2]>,
        meters: Arc<Meters>,
    ) -> Result<(Output, Reader)> {
        let dev = find_input_device(name)?;
        let config = cpal::StreamConfig {
            channels: in_channels,
            sample_rate: cpal::SampleRate(sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };
        let in_ch = in_channels as usize;
        let capacity = (sample_rate as usize * 2 * 400 / 1000).max(4); // stéréo, ~400 ms
        let (mut prod, cons) = HeapRb::<f32>::new(capacity).split();
        let stop = Arc::new(AtomicBool::new(false));
        let (stop_thread, name_owned) = (stop.clone(), name.to_string());
        let mut sbuf = vec![0f32; 8192 * 2]; // scratch stéréo réutilisé
        let thread = std::thread::Builder::new()
            .name("cpal-in".into())
            .spawn(move || {
                let stream = match dev.build_input_stream(
                    &config,
                    move |data: &[f32], _| {
                        if in_ch == 0 {
                            return;
                        }
                        let i0 = sel[0].load(Ordering::Relaxed).min(in_ch - 1);
                        let i1 = sel[1].load(Ordering::Relaxed).min(in_ch - 1);
                        let frames = data.len() / in_ch;
                        let need = frames * 2;
                        if need > sbuf.len() {
                            sbuf.resize(need, 0.0);
                        }
                        for f in 0..frames {
                            let base = f * in_ch;
                            sbuf[f * 2] = data[base + i0];
                            sbuf[f * 2 + 1] = data[base + i1];
                        }
                        meters.ret.store_stereo(&sbuf[..need]);
                        // Plein : on jette (le tampon reste borné, la latence aussi).
                        let _ = prod.push_slice(&sbuf[..need]);
                    },
                    |e| error!("flux d'entrée CoreAudio : {e}"),
                    None,
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        error!("création du flux d'entrée CoreAudio « {name_owned} » : {e}");
                        return;
                    }
                };
                if let Err(e) = stream.play() {
                    error!("démarrage du flux d'entrée CoreAudio : {e}");
                    return;
                }
                info!(
                    "entrée CoreAudio « {name_owned} » : {in_channels} canaux @ {sample_rate} Hz"
                );
                while !stop_thread.load(Ordering::Relaxed) {
                    std::thread::park_timeout(std::time::Duration::from_millis(250));
                }
            })?;
        Ok((
            Output {
                stop,
                thread: Some(thread),
            },
            Reader {
                cons: Arc::new(Mutex::new(cons)),
            },
        ))
    }
}

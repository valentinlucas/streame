//! Audio : périphériques CoreAudio (Behringer Wing ou autre carte multicanal), bus d'habillage
//! et flux temps réel cpal (mixage, routage, VU-mètres dans les callbacks de la carte).

use cpal::traits::{DeviceTrait, HostTrait};
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::HeapRb;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{debug, warn};

#[derive(Debug, Clone)]
pub struct AudioDevice {
    pub name: String,
    /// « Audio/Sink » (sortie) ou « Audio/Source » (entrée).
    pub class: String,
    pub max_channels: Option<i32>,
}

/// Liste les périphériques audio CoreAudio (un même appareil apparaît en sortie et en entrée).
pub fn list_devices() -> Vec<AudioDevice> {
    let host = cpal::default_host();
    let mut out = Vec::new();
    let max_ch = |it: Option<Box<dyn Iterator<Item = cpal::SupportedStreamConfigRange>>>| {
        it.and_then(|cfgs| cfgs.map(|c| c.channels() as i32).max())
    };
    if let Ok(devs) = host.output_devices() {
        for d in devs {
            let Ok(name) = d.name() else { continue };
            let cfgs = d
                .supported_output_configs()
                .ok()
                .map(|c| Box::new(c) as Box<dyn Iterator<Item = _>>);
            out.push(AudioDevice {
                name,
                class: "Audio/Sink".into(),
                max_channels: max_ch(cfgs),
            });
        }
    } else {
        warn!("impossible d'énumérer les sorties audio");
    }
    if let Ok(devs) = host.input_devices() {
        for d in devs {
            let Ok(name) = d.name() else { continue };
            let cfgs = d
                .supported_input_configs()
                .ok()
                .map(|c| Box::new(c) as Box<dyn Iterator<Item = _>>);
            out.push(AudioDevice {
                name,
                class: "Audio/Source".into(),
                max_channels: max_ch(cfgs),
            });
        }
    }
    out
}

// ----------------------------------------------------------------------------------------------
// Bus d'habillage : somme des sons des vidéos d'habillage → anneau de sortie cpal
// ----------------------------------------------------------------------------------------------

/// Entrée d'un producteur sur le bus (une par vidéo d'habillage). Le producteur pousse du F32
/// stéréo entrelacé en avance ; le bus le consomme au rythme réel.
pub struct BusInput {
    prod: Mutex<ringbuf::HeapProd<f32>>,
}

impl BusInput {
    /// Pousse ce qui tient ; renvoie le nombre d'échantillons acceptés.
    pub fn push(&self, samples: &[f32]) -> usize {
        match self.prod.lock() {
            Ok(mut p) => p.push_slice(samples),
            Err(_) => 0,
        }
    }
}

/// Mélangeur des sons d'habillage. Un thread cadencé à 10 ms somme les entrées et pousse le
/// résultat (silence compris) dans l'anneau d'habillage de la sortie cpal — flux continu, donc
/// l'anneau reste amorcé et l'asservissement de dérive verrouillé. Remplace l'`audiomixer` +
/// `audiotestsrc` (silence) GStreamer.
pub struct BrandingBus {
    inputs: Arc<Mutex<Vec<ringbuf::HeapCons<f32>>>>,
    sample_rate: u32,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl BrandingBus {
    pub fn start(pusher: cpal_out::Pusher, sample_rate: u32) -> Self {
        let inputs: Arc<Mutex<Vec<ringbuf::HeapCons<f32>>>> = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (inputs_t, stop_t) = (inputs.clone(), stop.clone());
        let tick = Duration::from_millis(10);
        let n = (sample_rate as usize / 100).max(1) * 2; // 10 ms stéréo
        let thread = std::thread::Builder::new()
            .name("branding-bus".into())
            .spawn(move || {
                let mut sum = vec![0f32; n];
                let mut scratch = vec![0f32; n];
                let mut next = Instant::now();
                debug!("bus d'habillage démarré ({sample_rate} Hz, pas de 10 ms)");
                while !stop_t.load(Ordering::Relaxed) {
                    sum.iter_mut().for_each(|s| *s = 0.0);
                    if let Ok(mut ins) = inputs_t.lock() {
                        for c in ins.iter_mut() {
                            let got = c.pop_slice(&mut scratch);
                            for i in 0..got {
                                sum[i] += scratch[i];
                            }
                        }
                    }
                    pusher.push(&sum);
                    next += tick;
                    let now = Instant::now();
                    if next > now {
                        std::thread::sleep(next - now);
                    } else if now - next > Duration::from_millis(200) {
                        next = now; // grosse pause (veille…) : on ne rattrape pas
                    }
                }
            })
            .expect("thread bus d'habillage");
        Self {
            inputs,
            sample_rate,
            stop,
            thread: Some(thread),
        }
    }

    /// Nouvelle entrée (anneau d'une seconde).
    pub fn add_input(&self) -> BusInput {
        let cap = (self.sample_rate as usize * 2).max(4);
        let (prod, cons) = HeapRb::<f32>::new(cap).split();
        if let Ok(mut ins) = self.inputs.lock() {
            ins.push(cons);
        }
        BusInput {
            prod: Mutex::new(prod),
        }
    }
}

impl Drop for BrandingBus {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// E/S CoreAudio via cpal : le mixage (stream du téléphone + habillage), le routage vers les
/// canaux de la carte, les VU-mètres et la sélection des canaux de retour se font dans les
/// callbacks temps réel, à l'horloge exacte de la carte (une seule horloge, aucun autre framework
/// n'ouvre le périphérique).
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

    /// Poignée d'écriture (réseau via libopus, ou bus d'habillage). Le consommateur vit dans le
    /// thread temps réel CoreAudio et lit sans verrou.
    #[derive(Clone)]
    pub struct Pusher {
        prod: Arc<Mutex<Prod>>,
        /// Cible de remplissage (échantillons entrelacés) = latence visée.
        target: usize,
        capacity: usize,
        channels: usize,
        /// Compensation de dérive d'horloge (ré-échantillonnage asservi au remplissage).
        asrc: Arc<Mutex<Asrc>>,
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
    /// téléphone et habillage) sont alimentées via les `Pusher` renvoyés ; chacune
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
        let buffer_size = dev
            .default_output_config()
            .map(|c| low_latency_buffer(c.buffer_size()))
            .unwrap_or(cpal::BufferSize::Default);
        let config = cpal::StreamConfig {
            channels: out_channels,
            sample_rate: cpal::SampleRate(sample_rate),
            buffer_size,
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
        // Pré-tampon : on attend d'avoir accumulé ~50 ms avant de jouer une source, sinon la
        // moindre gigue vide l'anneau et le callback comble avec du silence → son haché. En cas
        // de sous-alimentation on repasse en pré-tampon (une coupure nette plutôt qu'un hachis).
        // En régime établi, l'asservissement de dérive du `Pusher` maintient ~70 ms, donc on ne
        // repasse jamais par ici.
        let prime = (sample_rate as usize * 2 * 50 / 1000).max(2);
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
            // Remplissage visé par l'asservissement de dérive (~70 ms) : au-dessus du pré-tampon
            // du callback (50 ms) pour ne jamais re-déclencher de sous-alimentation en régime
            // établi, assez bas pour rester à faible latence.
            target: (sample_rate as usize * 2 * 70 / 1000).max(2),
            capacity: cap,
            channels: 2,
            asrc: Arc::new(Mutex::new(Asrc::new())),
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

    /// Taille de tampon CoreAudio visée (trames par callback) : 256 @ 48 kHz ≈ 5,3 ms, contre
    /// 512 (~10,7 ms) par défaut. On ne l'impose que si la carte l'annonce comme supportée.
    const LOW_LATENCY_FRAMES: u32 = 256;

    fn low_latency_buffer(supported: &cpal::SupportedBufferSize) -> cpal::BufferSize {
        match supported {
            cpal::SupportedBufferSize::Range { min, max }
                if *min <= LOW_LATENCY_FRAMES && LOW_LATENCY_FRAMES <= *max =>
            {
                cpal::BufferSize::Fixed(LOW_LATENCY_FRAMES)
            }
            _ => cpal::BufferSize::Default,
        }
    }

    /// Écart maximal du rapport de ré-échantillonnage (±0,5 % : inaudible, et 25× la dérive
    /// typique de deux quartz).
    const MAX_DRIFT: f64 = 0.005;

    /// Ré-échantillonnage asynchrone = compensation de dérive d'horloge. La source (réseau via
    /// libopus, ou le bus d'habillage à l'horloge murale) et la carte tournent sur deux horloges 48 kHz
    /// indépendantes : sans compensation, l'anneau se vide ou se remplit lentement, jusqu'à la
    /// coupure ou au saut de trames. Ici le rapport de ré-échantillonnage est asservi en douceur
    /// au remplissage de l'anneau (cible ~70 ms) : interpolation linéaire, rapport lissé et borné,
    /// convergence en quelques secondes — c'est ce que font les moteurs audio WebRTC (NetEQ/ADM)
    /// et les sinks GStreamer (« clock slaving »). À rapport exactement 1, la sortie est identique
    /// à l'entrée (aucune perte de qualité en régime verrouillé).
    struct Asrc {
        /// Trames de sortie produites par trame d'entrée (≈ 1 ± MAX_DRIFT).
        ratio: f64,
        /// Position fractionnaire de lecture dans l'entrée virtuelle `[prev, x0, x1, …]`.
        phase: f64,
        /// Dernière trame d'entrée du bloc précédent (continuité de l'interpolation).
        prev: [f32; 2],
        started: bool,
        /// Sortie ré-échantillonnée du bloc courant (réutilisée, pas d'allocation en régime).
        buf: Vec<f32>,
    }

    impl Asrc {
        fn new() -> Self {
            Self {
                ratio: 1.0,
                phase: 1.0,
                prev: [0.0; 2],
                started: false,
                buf: Vec::with_capacity(8192),
            }
        }

        /// Asservit le rapport au remplissage : `err` = écart relatif à la cible (>0 : trop
        /// plein, la carte consomme moins vite → produire moins ; <0 : l'inverse).
        fn steer(&mut self, err: f64) {
            let wanted = (1.0 - 0.01 * err).clamp(1.0 - MAX_DRIFT, 1.0 + MAX_DRIFT);
            self.ratio += (wanted - self.ratio) * 0.05;
        }

        /// Ré-échantillonne un bloc stéréo entrelacé dans `self.buf`.
        fn process(&mut self, x: &[f32]) -> &[f32] {
            let n = x.len() / 2;
            self.buf.clear();
            if n == 0 {
                return &self.buf;
            }
            if !self.started {
                self.prev = [x[0], x[1]];
                self.phase = 1.0;
                self.started = true;
            }
            let step = 1.0 / self.ratio;
            let frame = |i: usize| -> [f32; 2] {
                if i == 0 {
                    self.prev
                } else {
                    [x[(i - 1) * 2], x[(i - 1) * 2 + 1]]
                }
            };
            let mut pos = self.phase;
            while pos < n as f64 {
                let i = pos as usize;
                let frac = (pos - i as f64) as f32;
                let (a, b) = (frame(i), frame(i + 1));
                self.buf.push(a[0] + (b[0] - a[0]) * frac);
                self.buf.push(a[1] + (b[1] - a[1]) * frac);
                pos += step;
            }
            self.phase = pos - n as f64;
            self.prev = [x[(n - 1) * 2], x[(n - 1) * 2 + 1]];
            &self.buf
        }
    }

    impl Pusher {
        /// Pousse des échantillons F32 stéréo entrelacés, après compensation de dérive (voir
        /// [`Asrc`]). Plafond dur en dernier recours (anneau quasi plein après un décrochage) :
        /// on ne garde que la fin du bloc.
        pub fn push(&self, samples: &[f32]) {
            let (Ok(mut prod), Ok(mut asrc)) = (self.prod.lock(), self.asrc.lock()) else {
                return;
            };
            let occupied = prod.occupied_len();
            let err = (occupied as f64 - self.target as f64) / self.target.max(1) as f64;
            asrc.steer(err);
            let out = asrc.process(samples);
            let free = self.capacity.saturating_sub(prod.occupied_len());
            if out.len() > free {
                let ch = self.channels.max(1);
                let start = (out.len() - free) - ((out.len() - free) % ch);
                let _ = prod.push_slice(&out[start..]);
                warn!("sortie CoreAudio : tampon plein, début de bloc sauté (décrochage)");
                return;
            }
            let _ = prod.push_slice(out);
        }
    }

    // ------------------------------------------------------------------------------------------
    // ENTRÉE CoreAudio (retour de la carte vers le téléphone)
    // ------------------------------------------------------------------------------------------

    type Cons = ringbuf::HeapCons<f32>;

    /// Lecteur du retour (côté encodeur Opus). Le producteur est le thread temps réel d'entrée
    /// CoreAudio, sans verrou ; ici on lit sous verrou (tâche d'encodage, non temps réel).
    #[derive(Clone)]
    pub struct Reader {
        cons: Arc<Mutex<Cons>>,
        /// Au-delà, on jette les échantillons les plus anciens (le consommateur a décroché :
        /// tâche d'encodage en retard, session absente) pour borner la latence du retour.
        drift_max: usize,
    }

    impl Reader {
        /// Extrait exactement une trame (`out.len()` échantillons F32 stéréo entrelacés) si elle
        /// est disponible et renvoie `true` ; sinon ne touche pas `out` et renvoie `false`.
        /// L'appelant encode donc au rythme exact de la carte (pas d'horloge murale, pas de
        /// trames complétées en silence — c'était la cause du retour haché).
        pub fn pull_frame(&self, out: &mut [f32]) -> bool {
            let Ok(mut c) = self.cons.lock() else {
                return false;
            };
            // Si le consommateur a décroché, on saute le plus ancien pour repartir à ~2 trames.
            let occupied = c.occupied_len();
            if occupied > self.drift_max {
                let keep = (out.len() * 2) & !1;
                let mut skip = occupied.saturating_sub(keep) & !1;
                let mut junk = [0f32; 1024];
                while skip > 0 {
                    let n = junk.len().min(skip);
                    let got = c.pop_slice(&mut junk[..n]);
                    if got == 0 {
                        break;
                    }
                    skip -= got;
                }
            }
            if c.occupied_len() < out.len() {
                return false;
            }
            c.pop_slice(out) == out.len()
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
        let buffer_size = dev
            .default_input_config()
            .map(|c| low_latency_buffer(c.buffer_size()))
            .unwrap_or(cpal::BufferSize::Default);
        let config = cpal::StreamConfig {
            channels: in_channels,
            sample_rate: cpal::SampleRate(sample_rate),
            buffer_size,
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
        let drift_max = (sample_rate as usize * 2 * 200 / 1000).max(4); // ~200 ms
        Ok((
            Output {
                stop,
                thread: Some(thread),
            },
            Reader {
                cons: Arc::new(Mutex::new(cons)),
                drift_max,
            },
        ))
    }
}

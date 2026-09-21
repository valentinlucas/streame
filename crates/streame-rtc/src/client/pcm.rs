//! Anneaux PCM entre le moteur audio du téléphone et les codecs Opus (F32 stéréo 48 kHz), et
//! ré-échantillonnage. Utilisés par cpal (`cpal_io`, bancs et Mac) comme par le pont
//! [`PcmBridge`] de l'app iOS (`AVAudioEngine`, avec le traitement vocal Apple : annulation
//! d'écho, gain automatique, réduction de bruit).
//!
//! Les callbacks temps réel ne font que copier : aucune allocation (tampons dimensionnés pour
//! [`MAX_BLOCK_FRAMES`] à la création), aucun journal, aucun verrou partagé avec un autre thread
//! (les mutex ci-dessous ne sont pris que par le thread qui possède le côté correspondant de
//! l'anneau). L'encodage Opus est cadencé par la capture : le producteur réveille l'encodeur
//! (`Notify`) dès qu'une trame de 20 ms est disponible.

use super::vtenc::Counters;
use crate::opus::{PcmSink, PcmSource, FRAME_SAMPLES, SAMPLE_RATE};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::HeapRb;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

/// Plus grand bloc accepté par les callbacks (trames par appel). Sur iOS le tampon d'E/S est
/// au plus de 4096 trames ; au-delà le bloc est tronqué plutôt que de réallouer.
pub const MAX_BLOCK_FRAMES: usize = 4096;
/// Fréquence la plus basse attendue d'un périphérique (Bluetooth HFP) : borne du
/// ré-échantillonnage pour dimensionner les tampons.
const MIN_DEVICE_RATE: usize = 8_000;
/// Capacité (échantillons stéréo entrelacés) d'un bloc ré-échantillonné dans le pire cas.
const MAX_RESAMPLED: usize = MAX_BLOCK_FRAMES * 2 * (SAMPLE_RATE as usize / MIN_DEVICE_RATE) + 4;

/// Ré-échantillonneur linéaire tout simple (fréquence du périphérique ≠ 48 kHz).
pub struct Resampler {
    /// Entrée → sortie : trames d'entrée par trame de sortie.
    pub ratio: f64,
    pos: f64,
    last: [f32; 2],
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        Self { ratio: from as f64 / to as f64, pos: 0.0, last: [0.0; 2] }
    }

    /// `input` et la sortie sont stéréo entrelacés.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let frames = input.len() / 2;
        if frames == 0 {
            return;
        }
        // Position 0 = `last` (dernière trame du bloc précédent), 1..=frames = ce bloc.
        while self.pos < frames as f64 {
            let i = self.pos.floor() as usize;
            let t = (self.pos - i as f64) as f32;
            let a = if i == 0 { self.last } else { [input[(i - 1) * 2], input[(i - 1) * 2 + 1]] };
            let b = [input[i * 2], input[i * 2 + 1]];
            out.push(a[0] + (b[0] - a[0]) * t);
            out.push(a[1] + (b[1] - a[1]) * t);
            self.pos += self.ratio;
        }
        self.pos -= frames as f64;
        self.last = [input[(frames - 1) * 2], input[(frames - 1) * 2 + 1]];
    }
}

/// Lecteur du micro (source de l'encodeur Opus). Producteur : le moteur audio.
pub struct MicReader {
    cons: Mutex<ringbuf::HeapCons<f32>>,
    /// Au-delà, on jette le plus ancien (l'encodeur a décroché) pour borner la latence.
    drift_max: usize,
    mic_on: Arc<AtomicBool>,
    /// Signalé par le producteur quand une trame complète est disponible.
    notify: Arc<Notify>,
}

impl PcmSource for MicReader {
    fn pull_frame(&self, out: &mut [f32]) -> bool {
        let Ok(mut c) = self.cons.lock() else {
            return false;
        };
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
        let ok = c.pop_slice(out) == out.len();
        if ok && !self.mic_on.load(Ordering::Relaxed) {
            // Micro coupé : on continue d'envoyer (silence) pour garder le flux vivant.
            out.iter_mut().for_each(|s| *s = 0.0);
        }
        ok
    }

    fn notify(&self) -> Option<Arc<Notify>> {
        Some(self.notify.clone())
    }
}

/// Écrivain vers la sortie (puits du décodeur Opus). Consommateur : le moteur audio.
pub struct SpeakerWriter {
    prod: Mutex<ringbuf::HeapProd<f32>>,
    spk_on: Arc<AtomicBool>,
    /// Latence maximale tolérée (échantillons stéréo) : au-delà on jette la trame.
    max_fill: usize,
}

impl PcmSink for SpeakerWriter {
    fn push(&self, samples: &[f32]) {
        if !self.spk_on.load(Ordering::Relaxed) {
            return;
        }
        if let Ok(mut p) = self.prod.lock() {
            if p.occupied_len() + samples.len() > self.max_fill {
                return; // le moteur ne suit plus (route audio en cours de changement…)
            }
            let _ = p.push_slice(samples);
        }
    }
}

/// Les deux anneaux d'une session audio : micro (vers Opus) et sortie (depuis Opus).
pub struct Rings {
    pub mic: Arc<MicReader>,
    pub speaker: Arc<SpeakerWriter>,
    pub mic_prod: ringbuf::HeapProd<f32>,
    pub spk_cons: ringbuf::HeapCons<f32>,
    /// À signaler (`notify_one`) après avoir poussé du micro quand l'anneau contient au moins
    /// [`FRAME_SAMPLES`] échantillons : réveille l'encodeur Opus sans scrutation.
    pub mic_notify: Arc<Notify>,
}

pub fn rings(mic_on: Arc<AtomicBool>, spk_on: Arc<AtomicBool>) -> Rings {
    let (mic_prod, mic_cons) = HeapRb::<f32>::new(SAMPLE_RATE as usize * 2 * 400 / 1000).split();
    let (spk_prod, spk_cons) = HeapRb::<f32>::new(SAMPLE_RATE as usize * 2 * 400 / 1000).split();
    let mic_notify = Arc::new(Notify::new());
    Rings {
        mic: Arc::new(MicReader {
            cons: Mutex::new(mic_cons),
            drift_max: SAMPLE_RATE as usize * 2 * 80 / 1000, // ~80 ms
            mic_on,
            notify: mic_notify.clone(),
        }),
        speaker: Arc::new(SpeakerWriter {
            prod: Mutex::new(spk_prod),
            spk_on,
            max_fill: SAMPLE_RATE as usize * 2 * 200 / 1000, // ~200 ms
        }),
        mic_prod,
        spk_cons,
        mic_notify,
    }
}

/// Convertit un bloc entrelacé (1 ou 2 canaux, `rate` Hz) en stéréo 48 kHz, avec un
/// ré-échantillonneur conservé entre les appels.
pub struct ToStereo48k {
    resampler: Option<Resampler>,
    rate: u32,
    stereo: Vec<f32>,
    out: Vec<f32>,
}

impl ToStereo48k {
    pub fn new() -> Self {
        Self {
            resampler: None,
            rate: 0,
            stereo: Vec::with_capacity(MAX_BLOCK_FRAMES * 2),
            out: Vec::with_capacity(MAX_RESAMPLED),
        }
    }

    /// Sans allocation tant que le bloc ne dépasse pas [`MAX_BLOCK_FRAMES`] (tronqué sinon).
    pub fn convert(&mut self, input: &[f32], channels: usize, rate: u32) -> &[f32] {
        if channels == 0 || input.is_empty() || rate < MIN_DEVICE_RATE as u32 {
            return &[];
        }
        if rate != self.rate {
            self.rate = rate;
            self.resampler = (rate != SAMPLE_RATE).then(|| Resampler::new(rate, SAMPLE_RATE));
        }
        let frames = (input.len() / channels).min(MAX_BLOCK_FRAMES);
        self.stereo.clear();
        for f in 0..frames {
            let l = input[f * channels];
            let r = if channels > 1 { input[f * channels + 1] } else { l };
            self.stereo.push(l);
            self.stereo.push(r);
        }
        match self.resampler.as_mut() {
            Some(rs) => {
                self.out.clear();
                rs.process(&self.stereo, &mut self.out);
                &self.out
            }
            None => &self.stereo,
        }
    }
}

impl Default for ToStereo48k {
    fn default() -> Self {
        Self::new()
    }
}

/// Lit l'anneau de sortie (stéréo 48 kHz) et produit `frames` trames entrelacées à
/// `channels` canaux et `rate` Hz, pré-tampon ~60 ms, silence en sous-alimentation.
pub struct FromStereo48k {
    resampler: Option<Resampler>,
    rate: u32,
    primed: bool,
    prebuffer: usize,
    pulled: Vec<f32>,
    resampled: Vec<f32>,
    /// Sous-alimentations (re-tamponnages) depuis la création : à lire hors temps réel.
    underruns: u32,
}

impl FromStereo48k {
    pub fn new() -> Self {
        Self {
            resampler: None,
            rate: 0,
            primed: false,
            prebuffer: SAMPLE_RATE as usize * 2 * 60 / 1000,
            pulled: Vec::with_capacity(MAX_RESAMPLED),
            resampled: Vec::with_capacity(MAX_BLOCK_FRAMES * 2 + 4),
            underruns: 0,
        }
    }

    pub fn underruns(&self) -> u32 {
        self.underruns
    }

    /// Renvoie `true` si du son (pas seulement du silence) a été écrit. Sans allocation ni
    /// journal (callback temps réel) ; au-delà de [`MAX_BLOCK_FRAMES`] trames, le reste du bloc
    /// est du silence.
    pub fn fill(
        &mut self,
        cons: &mut ringbuf::HeapCons<f32>,
        out: &mut [f32],
        channels: usize,
        rate: u32,
    ) -> bool {
        let channels = channels.max(1);
        let total_frames = out.len() / channels;
        let frames = total_frames.min(MAX_BLOCK_FRAMES);
        if rate < MIN_DEVICE_RATE as u32 {
            out.iter_mut().for_each(|s| *s = 0.0);
            return false;
        }
        if rate != self.rate {
            self.rate = rate;
            self.resampler = (rate != SAMPLE_RATE).then(|| Resampler::new(SAMPLE_RATE, rate));
        }
        if !self.primed && cons.occupied_len() >= self.prebuffer {
            self.primed = true;
        }
        let need_in = match self.resampler.as_ref() {
            Some(rs) => ((frames as f64) * rs.ratio).ceil() as usize + 1,
            None => frames,
        };
        self.pulled.clear();
        self.pulled.resize(need_in * 2, 0.0);
        let mut got = 0;
        if self.primed {
            got = cons.pop_slice(&mut self.pulled);
            if got < self.pulled.len() {
                self.pulled[got..].iter_mut().for_each(|s| *s = 0.0);
                if got == 0 {
                    self.primed = false;
                    self.underruns = self.underruns.wrapping_add(1);
                }
            }
        }
        let src: &[f32] = match self.resampler.as_mut() {
            Some(rs) => {
                self.resampled.clear();
                rs.process(&self.pulled, &mut self.resampled);
                &self.resampled
            }
            None => &self.pulled,
        };
        for f in 0..total_frames {
            let (l, r) = if f < frames && f * 2 + 1 < src.len() { (src[f * 2], src[f * 2 + 1]) } else { (0.0, 0.0) };
            for c in 0..channels {
                out[f * channels + c] = if c & 1 == 0 { l } else { r };
            }
        }
        got > 0
    }
}

impl Default for FromStereo48k {
    fn default() -> Self {
        Self::new()
    }
}

/// Pont pour un moteur audio externe (AVAudioEngine dans l'app iOS) : le moteur **pousse** le
/// micro et **tire** la sortie depuis ses callbacks temps réel ; Opus reste côté Rust.
pub struct PcmBridge {
    pub mic: Arc<MicReader>,
    pub speaker: Arc<SpeakerWriter>,
    /// Pris uniquement par le callback d'entrée du moteur audio (jamais contendu).
    mic_in: Mutex<(ringbuf::HeapProd<f32>, ToStereo48k)>,
    /// Pris uniquement par le callback de rendu du moteur audio (jamais contendu).
    spk_out: Mutex<(ringbuf::HeapCons<f32>, FromStereo48k)>,
    mic_notify: Arc<Notify>,
    counters: Arc<Counters>,
}

impl PcmBridge {
    pub fn new(mic_on: Arc<AtomicBool>, spk_on: Arc<AtomicBool>, counters: Arc<Counters>) -> Self {
        let r = rings(mic_on, spk_on);
        Self {
            mic: r.mic,
            speaker: r.speaker,
            mic_in: Mutex::new((r.mic_prod, ToStereo48k::new())),
            spk_out: Mutex::new((r.spk_cons, FromStereo48k::new())),
            mic_notify: r.mic_notify,
            counters,
        }
    }

    /// Bloc du micro, entrelacé, 1 ou 2 canaux, à `rate` Hz (callback temps réel).
    pub fn push_mic(&self, input: &[f32], channels: usize, rate: u32) {
        let Ok(mut g) = self.mic_in.try_lock() else { return };
        let (prod, conv) = &mut *g;
        let data = conv.convert(input, channels, rate);
        let _ = prod.push_slice(data); // plein : on jette (latence bornée)
        if prod.occupied_len() >= FRAME_SAMPLES {
            self.mic_notify.notify_one();
        }
    }

    /// Remplit un bloc de sortie entrelacé (`channels` canaux, `rate` Hz). `true` si du son.
    /// Callback temps réel : silence si le verrou n'est pas libre (jamais le cas en pratique).
    pub fn pull_speaker(&self, out: &mut [f32], channels: usize, rate: u32) -> bool {
        match self.spk_out.try_lock() {
            Ok(mut g) => {
                let (cons, conv) = &mut *g;
                let ok = conv.fill(cons, out, channels, rate);
                self.counters.audio_underruns.store(conv.underruns(), Ordering::Relaxed);
                ok
            }
            Err(_) => {
                out.iter_mut().for_each(|s| *s = 0.0);
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bridge() -> PcmBridge {
        PcmBridge::new(
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(true)),
            Arc::new(Counters::default()),
        )
    }

    #[test]
    fn convertisseurs_sans_reallocation_au_bloc_maximal() {
        let mut to = ToStereo48k::new();
        let cap = (to.stereo.capacity(), to.out.capacity());
        let block = vec![0.1f32; MAX_BLOCK_FRAMES * 2];
        let n = to.convert(&block, 2, 8_000).len();
        assert!(n > 0 && n <= MAX_RESAMPLED);
        assert_eq!(cap, (to.stereo.capacity(), to.out.capacity()), "réallocation en temps réel");

        let mut from = FromStereo48k::new();
        let cap = (from.pulled.capacity(), from.resampled.capacity());
        let (mut prod, mut cons) = HeapRb::<f32>::new(SAMPLE_RATE as usize * 2).split();
        let _ = prod.push_slice(&vec![0.5f32; SAMPLE_RATE as usize * 2]);
        let mut out = vec![0f32; (MAX_BLOCK_FRAMES + 100) * 2];
        assert!(from.fill(&mut cons, &mut out, 2, 8_000));
        assert_eq!(cap, (from.pulled.capacity(), from.resampled.capacity()), "réallocation en temps réel");
        assert_eq!(out[(MAX_BLOCK_FRAMES + 50) * 2], 0.0, "au-delà du bloc maximal : silence");
    }

    #[test]
    fn sous_alimentation_comptee_sans_journal() {
        let bridge = bridge();
        let mut out = vec![0f32; 480 * 2];
        bridge.speaker.push(&vec![0.5f32; SAMPLE_RATE as usize * 2 / 10]);
        assert!(bridge.pull_speaker(&mut out, 2, 48_000));
        for _ in 0..12 {
            bridge.pull_speaker(&mut out, 2, 48_000);
        }
        assert_eq!(bridge.counters.audio_underruns.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn reechantillonnage_44k_vers_48k_conserve_la_duree() {
        let mut rs = Resampler::new(44_100, 48_000);
        let mut out = Vec::new();
        let input: Vec<f32> = (0..441 * 2).map(|i| (i / 2) as f32).collect(); // 10 ms
        rs.process(&input, &mut out);
        let frames = out.len() / 2;
        assert!((479..=481).contains(&frames), "{frames} trames pour 10 ms");
        for w in out.chunks(2).collect::<Vec<_>>().windows(2) {
            assert!(w[1][0] >= w[0][0]);
        }
    }

    #[test]
    fn pont_micro_mono_24k_vers_trames_opus() {
        let bridge = bridge();
        // 40 ms de mono à 24 kHz → 40 ms stéréo 48 kHz = 2 trames de 20 ms.
        let block: Vec<f32> = (0..960).map(|i| i as f32 / 960.0).collect();
        bridge.push_mic(&block, 1, 24_000);
        let mut frame = vec![0f32; 960 * 2];
        assert!(bridge.mic.pull_frame(&mut frame));
        assert!(bridge.mic.pull_frame(&mut frame));
        assert!(!bridge.mic.pull_frame(&mut frame), "pas plus de 2 trames");
        assert!(frame[0] <= frame[2]);
    }

    #[test]
    fn pont_sortie_silence_puis_son() {
        let bridge = bridge();
        let mut out = vec![0f32; 480 * 2];
        assert!(!bridge.pull_speaker(&mut out, 2, 48_000), "anneau vide : silence");
        let tone: Vec<f32> = vec![0.5; 48_000 * 2 / 10]; // 100 ms
        bridge.speaker.push(&tone);
        assert!(bridge.pull_speaker(&mut out, 2, 48_000));
        assert!(out.iter().all(|s| (*s - 0.5).abs() < 1e-6));
        let mut mono = vec![0f32; 240];
        assert!(bridge.pull_speaker(&mut mono, 1, 24_000));
    }
}

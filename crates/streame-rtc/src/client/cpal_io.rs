//! Audio du téléphone par **cpal** (CoreAudio), pour les bancs et les plateformes sans moteur
//! audio fourni par l'app : micro → anneau → encodeur Opus ; décodeur Opus → anneau → sortie.
//! L'app iOS n'utilise pas ce module : elle passe par `AVAudioEngine` (traitement vocal Apple)
//! et le pont [`super::pcm::PcmBridge`].

use super::pcm::{self, FromStereo48k, MicReader, SpeakerWriter, ToStereo48k};
use crate::opus::FRAME_SAMPLES;
use ringbuf::traits::{Observer, Producer};
use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info};

/// Flux cpal d'entrée et de sortie, chacun sur son thread (le `Stream` y reste vivant).
pub struct CpalAudio {
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
    pub mic: Arc<MicReader>,
    pub speaker: Arc<SpeakerWriter>,
}

impl CpalAudio {
    pub fn start(mic_on: Arc<AtomicBool>, spk_on: Arc<AtomicBool>) -> Result<Self> {
        let host = cpal::default_host();
        let stop = Arc::new(AtomicBool::new(false));

        // --- Micro ---------------------------------------------------------------------------
        let in_dev = host
            .default_input_device()
            .ok_or_else(|| anyhow!("aucune entrée audio par défaut"))?;
        let in_cfg = in_dev.default_input_config().context("config d'entrée")?;
        let rings = pcm::rings(mic_on, spk_on);
        let mic = rings.mic;
        let in_thread = spawn_input(in_dev, in_cfg, rings.mic_prod, rings.mic_notify, stop.clone())?;

        // --- Sortie --------------------------------------------------------------------------
        let out_dev = host
            .default_output_device()
            .ok_or_else(|| anyhow!("aucune sortie audio par défaut"))?;
        let out_cfg = out_dev.default_output_config().context("config de sortie")?;
        let speaker = rings.speaker;
        let out_thread = spawn_output(out_dev, out_cfg, rings.spk_cons, stop.clone())?;

        Ok(Self { stop, threads: vec![in_thread, out_thread], mic, speaker })
    }
}

impl Drop for CpalAudio {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

fn spawn_input(
    dev: cpal::Device,
    cfg: cpal::SupportedStreamConfig,
    mut prod: ringbuf::HeapProd<f32>,
    notify: Arc<tokio::sync::Notify>,
    stop: Arc<AtomicBool>,
) -> Result<std::thread::JoinHandle<()>> {
    let channels = cfg.channels() as usize;
    let rate = cfg.sample_rate().0;
    let format = cfg.sample_format();
    let stream_cfg: cpal::StreamConfig = cfg.into();
    let name = dev.name().unwrap_or_else(|_| "?".into());
    let mut conv = ToStereo48k::new();
    let mut block: Vec<f32> = Vec::with_capacity(8192);
    // Convertit le bloc du périphérique en stéréo 48 kHz et le pousse dans l'anneau.
    let mut deliver = move |frames: usize, sample: &dyn Fn(usize, usize) -> f32| {
        if channels == 0 {
            return;
        }
        block.clear();
        for f in 0..frames {
            for c in 0..channels.min(2) {
                block.push(sample(f, c));
            }
        }
        let data = conv.convert(&block, channels.min(2), rate);
        let _ = prod.push_slice(data); // plein : on jette (latence bornée)
        if prod.occupied_len() >= FRAME_SAMPLES {
            notify.notify_one();
        }
    };
    let err_fn = |e| error!("flux d'entrée cpal : {e}");
    let thread = std::thread::Builder::new()
        .name("cpal-mic".into())
        .spawn(move || {
            let stream = match format {
                cpal::SampleFormat::F32 => dev.build_input_stream(
                    &stream_cfg,
                    move |data: &[f32], _| deliver(data.len() / channels, &|f, c| data[f * channels + c]),
                    err_fn,
                    None,
                ),
                cpal::SampleFormat::I16 => dev.build_input_stream(
                    &stream_cfg,
                    move |data: &[i16], _| {
                        deliver(data.len() / channels, &|f, c| data[f * channels + c] as f32 / 32768.0)
                    },
                    err_fn,
                    None,
                ),
                cpal::SampleFormat::I32 => dev.build_input_stream(
                    &stream_cfg,
                    move |data: &[i32], _| {
                        deliver(data.len() / channels, &|f, c| data[f * channels + c] as f32 / 2147483648.0)
                    },
                    err_fn,
                    None,
                ),
                other => {
                    error!("format d'entrée cpal non géré : {other:?}");
                    return;
                }
            };
            let stream = match stream {
                Ok(s) => s,
                Err(e) => {
                    error!("création du flux micro « {name} » : {e}");
                    return;
                }
            };
            if let Err(e) = stream.play() {
                error!("démarrage du flux micro : {e}");
                return;
            }
            info!("micro « {name} » : {channels} canaux @ {rate} Hz ({format:?})");
            while !stop.load(Ordering::Relaxed) {
                std::thread::park_timeout(Duration::from_millis(200));
            }
            drop(stream);
        })
        .context("thread cpal-mic")?;
    Ok(thread)
}

fn spawn_output(
    dev: cpal::Device,
    cfg: cpal::SupportedStreamConfig,
    mut cons: ringbuf::HeapCons<f32>,
    stop: Arc<AtomicBool>,
) -> Result<std::thread::JoinHandle<()>> {
    let channels = cfg.channels() as usize;
    let rate = cfg.sample_rate().0;
    let format = cfg.sample_format();
    let stream_cfg: cpal::StreamConfig = cfg.into();
    let name = dev.name().unwrap_or_else(|_| "?".into());
    let mut conv = FromStereo48k::new();
    // Remplit le bloc (`channels` canaux, fréquence du périphérique) depuis l'anneau.
    let mut fill = move |out: &mut [f32]| {
        conv.fill(&mut cons, out, channels, rate);
    };
    let err_fn = |e| error!("flux de sortie cpal : {e}");
    let thread = std::thread::Builder::new()
        .name("cpal-spk".into())
        .spawn(move || {
            let mut scratch: Vec<f32> = Vec::with_capacity(4096);
            let stream = match format {
                cpal::SampleFormat::F32 => dev.build_output_stream(
                    &stream_cfg,
                    move |data: &mut [f32], _| fill(data),
                    err_fn,
                    None,
                ),
                cpal::SampleFormat::I16 => dev.build_output_stream(
                    &stream_cfg,
                    move |data: &mut [i16], _| {
                        scratch.clear();
                        scratch.resize(data.len(), 0.0);
                        fill(&mut scratch);
                        for (d, s) in data.iter_mut().zip(scratch.iter()) {
                            *d = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                        }
                    },
                    err_fn,
                    None,
                ),
                other => {
                    error!("format de sortie cpal non géré : {other:?}");
                    return;
                }
            };
            let stream = match stream {
                Ok(s) => s,
                Err(e) => {
                    error!("création du flux de sortie « {name} » : {e}");
                    return;
                }
            };
            if let Err(e) = stream.play() {
                error!("démarrage du flux de sortie : {e}");
                return;
            }
            info!("sortie « {name} » : {channels} canaux @ {rate} Hz ({format:?})");
            while !stop.load(Ordering::Relaxed) {
                std::thread::park_timeout(Duration::from_millis(200));
            }
            drop(stream);
        })
        .context("thread cpal-spk")?;
    Ok(thread)
}

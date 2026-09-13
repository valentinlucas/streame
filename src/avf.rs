//! Lecture des fichiers vidéo d'habillage par **AVFoundation** (décodage matériel), sans GStreamer.
//!
//! `AVAssetReader` décode le fichier — VideoToolbox pour l'image, AudioToolbox pour le son — et
//! livre :
//! - des `CVPixelBuffer` NV12 adossés à une **IOSurface**, que le rendu importe comme textures
//!   Metal sans copie (voir `frame.rs` / `render.rs`) ;
//! - du PCM F32 stéréo 48 kHz (ré-échantillonné par AVFoundation si besoin), poussé dans le bus
//!   d'habillage (`audio::BrandingBus`), mixé puis envoyé à la carte par cpal.
//!
//! Un thread par fichier cadence la présentation sur l'horloge murale ; en boucle, le lecteur est
//! recréé à chaque passage en gardant un temps continu (pas de coupure ni de saut).
#![allow(deprecated)] // noms C d'Apple conservés (CMSampleBufferGet…, CMTimeGetSeconds…)

use crate::audio::BusInput;
use crate::frame::{FrameSlot, SurfaceFrame};
use anyhow::{anyhow, Context, Result};
use objc2::runtime::AnyObject;
use objc2_av_foundation::{
    AVAssetReader, AVAssetReaderAudioMixOutput, AVAssetReaderStatus, AVAssetReaderTrackOutput,
    AVMediaTypeAudio, AVMediaTypeVideo, AVURLAsset,
};
use objc2_avf_audio::{
    AVFormatIDKey, AVLinearPCMBitDepthKey, AVLinearPCMIsBigEndianKey, AVLinearPCMIsFloatKey,
    AVLinearPCMIsNonInterleaved, AVNumberOfChannelsKey, AVSampleRateKey,
};
use objc2_core_media::{CMBlockBufferCopyDataBytes, CMBlockBufferGetDataLength, CMTimeGetSeconds};
use objc2_core_video::{
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_foundation::{NSDictionary, NSNumber, NSString, NSURL};
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// Retard appliqué à l'image pour l'aligner sur le son : le PCM traverse le bus d'habillage
/// (10 ms) puis l'anneau de sortie cpal (~70 ms de remplissage visé) avant la carte.
const AV_DELAY: Duration = Duration::from_millis(80);
/// Avance de lecture du son sur l'image (le bus le consomme au fil de l'eau).
const AUDIO_LOOKAHEAD_S: f64 = 0.3;
/// Au-delà de ce retard de présentation, l'image est sautée (décodage plus lent que le direct).
const MAX_LATE: Duration = Duration::from_millis(200);
/// `kAudioFormatLinearPCM` ('lpcm').
const AUDIO_FORMAT_LINEAR_PCM: u32 = 0x6c70_636d;

/// Lecteur d'un fichier vidéo d'habillage ; le thread s'arrête et est joint au `Drop`.
pub struct Player {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Informations lues à l'ouverture (pour le journal et la validation de la configuration).
struct Probe {
    duration_s: f64,
    width: u32,
    height: u32,
    fps: f32,
    has_audio: bool,
}

fn file_url(path: &Path) -> Retained<NSURL> {
    let s = NSString::from_str(&path.to_string_lossy());
    NSURL::fileURLWithPath(&s)
}

use objc2::rc::Retained;

fn open_asset(path: &Path) -> Retained<AVURLAsset> {
    // SAFETY: URL de fichier valide, pas d'options.
    unsafe { AVURLAsset::URLAssetWithURL_options(&file_url(path), None) }
}

fn probe(path: &Path) -> Result<Probe> {
    let asset = open_asset(path);
    // SAFETY: statiques exportées par AVFoundation ; appels de lecture simples.
    let (video_type, audio_type) = unsafe {
        (
            AVMediaTypeVideo.context("AVMediaTypeVideo")?,
            AVMediaTypeAudio.context("AVMediaTypeAudio")?,
        )
    };
    let vtracks = unsafe { asset.tracksWithMediaType(video_type) };
    let track = vtracks
        .firstObject()
        .ok_or_else(|| anyhow!("aucune piste vidéo lisible dans {}", path.display()))?;
    let (size, fps, duration) = unsafe {
        (
            track.naturalSize(),
            track.nominalFrameRate(),
            CMTimeGetSeconds(asset.duration()),
        )
    };
    let has_audio = unsafe { asset.tracksWithMediaType(audio_type) }.count() > 0;
    Ok(Probe {
        duration_s: duration,
        width: size.width.max(0.0) as u32,
        height: size.height.max(0.0) as u32,
        fps,
        has_audio,
    })
}

/// Réglages de sortie vidéo : NV12 (format natif des décodeurs, zéro conversion), compatible
/// Metal, adossé à une IOSurface (importable par le GPU sans copie).
fn video_settings() -> Retained<NSDictionary<NSString, AnyObject>> {
    let format = NSNumber::numberWithUnsignedInt(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
    let yes = NSNumber::numberWithBool(true);
    let surface_props: Retained<NSDictionary<NSString, AnyObject>> =
        NSDictionary::from_slices::<NSString>(&[], &[]);
    // SAFETY: statiques exportées par CoreVideo ; CFString ↔ NSString sont interchangeables.
    let keys: [&NSString; 3] = unsafe {
        [
            kCVPixelBufferPixelFormatTypeKey.as_ref(),
            kCVPixelBufferMetalCompatibilityKey.as_ref(),
            kCVPixelBufferIOSurfacePropertiesKey.as_ref(),
        ]
    };
    let values: [&AnyObject; 3] = [&format, &yes, &surface_props];
    NSDictionary::from_slices(&keys, &values)
}

/// Réglages de sortie audio : PCM F32 stéréo entrelacé à la fréquence de la carte —
/// AVFoundation ré-échantillonne et mélange les pistes (matériel/DSP du système).
fn audio_settings(sample_rate: u32) -> Result<Retained<NSDictionary<NSString, AnyObject>>> {
    // SAFETY: statiques exportées par AVFAudio.
    let keys: [&NSString; 7] = unsafe {
        [
            AVFormatIDKey.context("AVFormatIDKey")?,
            AVSampleRateKey.context("AVSampleRateKey")?,
            AVNumberOfChannelsKey.context("AVNumberOfChannelsKey")?,
            AVLinearPCMBitDepthKey.context("AVLinearPCMBitDepthKey")?,
            AVLinearPCMIsFloatKey.context("AVLinearPCMIsFloatKey")?,
            AVLinearPCMIsBigEndianKey.context("AVLinearPCMIsBigEndianKey")?,
            AVLinearPCMIsNonInterleaved.context("AVLinearPCMIsNonInterleaved")?,
        ]
    };
    let format = NSNumber::numberWithUnsignedInt(AUDIO_FORMAT_LINEAR_PCM);
    let rate = NSNumber::numberWithDouble(sample_rate as f64);
    let channels = NSNumber::numberWithUnsignedInt(2);
    let depth = NSNumber::numberWithUnsignedInt(32);
    let yes = NSNumber::numberWithBool(true);
    let no = NSNumber::numberWithBool(false);
    let values: [&AnyObject; 7] = [&format, &rate, &channels, &depth, &yes, &no, &no];
    Ok(NSDictionary::from_slices(&keys, &values))
}

/// Démarre la lecture d'un fichier dans un thread dédié. L'image va dans `slot`, le son (s'il y
/// en a et qu'un bus est fourni) dans `audio`. Erreur immédiate si le fichier n'est pas lisible.
pub fn spawn(
    path: &Path,
    looped: bool,
    slot: Arc<FrameSlot>,
    audio: Option<BusInput>,
    sample_rate: u32,
) -> Result<Player> {
    let path: PathBuf = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let p = probe(&path).with_context(|| format!("vidéo illisible : {}", path.display()))?;
    info!(
        "vidéo d'habillage « {} » : {}x{} @ {:.1} fps, {:.1} s{}{} (AVFoundation)",
        path.display(),
        p.width,
        p.height,
        p.fps,
        p.duration_s,
        if p.has_audio { ", son" } else { ", muette" },
        if looped { ", en boucle" } else { "" }
    );
    if p.has_audio && audio.is_none() {
        debug!("vidéo « {} » : son ignoré (pas de sortie audio)", path.display());
    }
    let stop = Arc::new(AtomicBool::new(false));
    let stop_t = stop.clone();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let thread = std::thread::Builder::new()
        .name(format!("avf-{name}"))
        .spawn(move || {
            // Instant correspondant au temps média 0 du passage courant ; incrémenté de la durée
            // du fichier à chaque boucle → temps continu, image et son sans rupture.
            let mut origin = Instant::now();
            loop {
                if stop_t.load(Ordering::Relaxed) {
                    break;
                }
                match play_once(&path, &slot, audio.as_ref(), sample_rate, origin, &stop_t) {
                    Ok(duration_s) => {
                        if !looped {
                            debug!("vidéo « {name} » : fin de lecture (dernière image conservée)");
                            break;
                        }
                        let d = Duration::from_secs_f64(duration_s.max(0.001));
                        origin += d;
                        // Si la lecture a pris du retard (décodage lent), on resynchronise
                        // plutôt que de courir après.
                        if origin + Duration::from_millis(100) < Instant::now() {
                            origin = Instant::now();
                        }
                    }
                    Err(e) => {
                        warn!("vidéo « {name} » : {e:#}");
                        if !looped {
                            break;
                        }
                        // Fichier momentanément illisible : nouvel essai dans une seconde.
                        for _ in 0..10 {
                            if stop_t.load(Ordering::Relaxed) {
                                return;
                            }
                            std::thread::sleep(Duration::from_millis(100));
                        }
                        origin = Instant::now();
                    }
                }
            }
        })
        .context("thread de lecture vidéo")?;
    Ok(Player {
        stop,
        thread: Some(thread),
    })
}

/// Un passage complet sur le fichier. Renvoie la durée média du fichier (s).
fn play_once(
    path: &Path,
    slot: &FrameSlot,
    audio: Option<&BusInput>,
    sample_rate: u32,
    origin: Instant,
    stop: &AtomicBool,
) -> Result<f64> {
    let asset = open_asset(path);
    // SAFETY: statiques AVFoundation ; l'asset est valide.
    let (video_type, audio_type) = unsafe {
        (
            AVMediaTypeVideo.context("AVMediaTypeVideo")?,
            AVMediaTypeAudio.context("AVMediaTypeAudio")?,
        )
    };
    let duration_s = unsafe { CMTimeGetSeconds(asset.duration()) };
    let vtracks = unsafe { asset.tracksWithMediaType(video_type) };
    let vtrack = vtracks
        .firstObject()
        .ok_or_else(|| anyhow!("aucune piste vidéo"))?;
    let reader = unsafe { AVAssetReader::assetReaderWithAsset_error(&asset) }
        .map_err(|e| anyhow!("AVAssetReader : {}", e.localizedDescription()))?;

    // SAFETY: réglages de type correct (NSDictionary<NSString, NSNumber|NSDictionary>).
    let vout = unsafe {
        AVAssetReaderTrackOutput::assetReaderTrackOutputWithTrack_outputSettings(
            &vtrack,
            Some(&video_settings()),
        )
    };
    unsafe {
        vout.setAlwaysCopiesSampleData(false);
        reader.addOutput(&vout);
    }

    let atracks = unsafe { asset.tracksWithMediaType(audio_type) };
    let aout = match audio {
        Some(_) if atracks.count() > 0 => {
            let settings = audio_settings(sample_rate)?;
            let out = unsafe {
                AVAssetReaderAudioMixOutput::assetReaderAudioMixOutputWithAudioTracks_audioSettings(
                    &atracks,
                    Some(&settings),
                )
            };
            unsafe {
                out.setAlwaysCopiesSampleData(false);
                reader.addOutput(&out);
            }
            Some(out)
        }
        _ => None,
    };

    if !unsafe { reader.startReading() } {
        let why = unsafe { reader.error() }
            .map(|e| e.localizedDescription().to_string())
            .unwrap_or_default();
        return Err(anyhow!("démarrage de la lecture : {why}"));
    }

    let mut pcm: Vec<u8> = Vec::new();
    let mut audio_pending = None;
    let mut audio_done = aout.is_none();
    let mut frames: u64 = 0;
    let mut dropped: u64 = 0;

    // Lecture en pas à pas (une seule file dans AVAssetReader) : pour chaque image, on lit
    // d'abord le son jusqu'à un peu au-delà de son horodatage, puis on attend l'échéance.
    loop {
        if stop.load(Ordering::Relaxed) {
            unsafe { reader.cancelReading() };
            return Ok(duration_s);
        }
        let Some(vsb) = (unsafe { vout.copyNextSampleBuffer() }) else {
            break;
        };
        let vpts = unsafe { CMTimeGetSeconds(vsb.presentation_time_stamp()) };

        if let (Some(aout), Some(bus)) = (&aout, audio) {
            while !audio_done {
                let asb = match audio_pending.take() {
                    Some(s) => s,
                    None => match unsafe { aout.copyNextSampleBuffer() } {
                        Some(s) => s,
                        None => {
                            audio_done = true;
                            break;
                        }
                    },
                };
                let apts = unsafe { CMTimeGetSeconds(asb.presentation_time_stamp()) };
                if apts > vpts + AUDIO_LOOKAHEAD_S {
                    audio_pending = Some(asb);
                    break;
                }
                if let Some(bb) = unsafe { asb.data_buffer() } {
                    let len = unsafe { CMBlockBufferGetDataLength(&bb) };
                    if len > 0 {
                        pcm.resize(len, 0);
                        let st = unsafe {
                            CMBlockBufferCopyDataBytes(
                                &bb,
                                0,
                                len,
                                NonNull::new(pcm.as_mut_ptr() as *mut c_void).unwrap(),
                            )
                        };
                        if st == 0 {
                            let samples: &[f32] = bytemuck::cast_slice(&pcm[..len - len % 4]);
                            if !push_all(bus, samples, stop) {
                                unsafe { reader.cancelReading() };
                                return Ok(duration_s);
                            }
                        }
                    }
                }
            }
        }

        // Image : décodée en NV12 sur IOSurface → directement au rendu.
        let Some(image) = (unsafe { vsb.image_buffer() }) else {
            continue;
        };
        let Some(frame) = SurfaceFrame::from_pixel_buffer(image) else {
            continue;
        };
        let deadline = origin + Duration::from_secs_f64(vpts.max(0.0)) + AV_DELAY;
        if !wait_until(deadline, stop) {
            unsafe { reader.cancelReading() };
            return Ok(duration_s);
        }
        if Instant::now().saturating_duration_since(deadline) > MAX_LATE {
            dropped += 1;
            continue;
        }
        slot.push(Arc::new(frame));
        frames += 1;
    }

    // Reste du son après la dernière image.
    if let (Some(aout), Some(bus)) = (&aout, audio) {
        let mut rest = audio_pending.take();
        while !audio_done {
            let asb = match rest.take() {
                Some(s) => s,
                None => match unsafe { aout.copyNextSampleBuffer() } {
                    Some(s) => s,
                    None => break,
                },
            };
            if let Some(bb) = unsafe { asb.data_buffer() } {
                let len = unsafe { CMBlockBufferGetDataLength(&bb) };
                if len > 0 {
                    pcm.resize(len, 0);
                    let st = unsafe {
                        CMBlockBufferCopyDataBytes(
                            &bb,
                            0,
                            len,
                            NonNull::new(pcm.as_mut_ptr() as *mut c_void).unwrap(),
                        )
                    };
                    if st == 0 && !push_all(bus, bytemuck::cast_slice(&pcm[..len - len % 4]), stop) {
                        break;
                    }
                }
            }
        }
    }

    let status = unsafe { reader.status() };
    if status == AVAssetReaderStatus::Failed {
        let why = unsafe { reader.error() }
            .map(|e| e.localizedDescription().to_string())
            .unwrap_or_default();
        return Err(anyhow!("lecture interrompue : {why}"));
    }
    debug!(
        "vidéo « {} » : passage terminé ({frames} images, {dropped} sautées)",
        path.display()
    );
    // Fin du passage : on attend la fin réelle du média (le dernier son est déjà dans le bus).
    let end = origin + Duration::from_secs_f64(duration_s.max(0.0)) + AV_DELAY;
    wait_until(end, stop);
    Ok(duration_s)
}

/// Attend une échéance par petits pas (réactif à l'arrêt). `false` si arrêt demandé.
fn wait_until(deadline: Instant, stop: &AtomicBool) -> bool {
    loop {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        let now = Instant::now();
        if now >= deadline {
            return true;
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(50)));
    }
}

/// Pousse tout un bloc dans le bus, en attendant s'il est plein (le bus consomme au rythme
/// réel ; on est en avance d'au plus `AUDIO_LOOKAHEAD_S`). `false` si arrêt demandé.
fn push_all(bus: &BusInput, mut samples: &[f32], stop: &AtomicBool) -> bool {
    while !samples.is_empty() {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        let n = bus.push(samples);
        samples = &samples[n..];
        if !samples.is_empty() {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    true
}

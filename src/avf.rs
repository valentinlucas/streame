//! Lecture des fichiers vidéo d'habillage par **AVFoundation** (décodage matériel), sans GStreamer.
//!
//! `AVAssetReader` décode le fichier — VideoToolbox pour l'image, AudioToolbox pour le son — et
//! livre :
//! - des `CVPixelBuffer` NV12 adossés à une **IOSurface**, que le rendu importe comme textures
//!   Metal sans copie (voir `frame.rs` / `render.rs`) ;
//! - du PCM F32 stéréo 48 kHz (ré-échantillonné par AVFoundation si besoin), poussé dans le bus
//!   d'habillage (`audio::BrandingBus`), mixé puis envoyé à la carte par cpal.
//!
//! Un thread par calque cadence la présentation sur l'horloge murale et enchaîne les **segments**
//! d'une liste de lecture (par exemple un générique lu une fois, puis une boucle : deux fichiers,
//! ou une plage d'un seul fichier). En boucle, le lecteur est recréé à chaque passage en gardant
//! un temps continu (pas de coupure ni de saut). Le calque peut démarrer dès le lancement ou
//! seulement quand sa scène passe à l'antenne (`PlayerCtl::start`), et s'arrêter en la quittant.
//!
//! Les fichiers avec couche alpha (ProRes 4444…) sont décodés en BGRA (alpha direct, tel que
//! livré par AVFoundation) pour être superposés au flux du téléphone.
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
use objc2_core_foundation::{CFBoolean, CFNumber};
use objc2_core_media::{
    kCMFormatDescriptionExtension_ContainsAlphaChannel, CMBlockBufferCopyDataBytes,
    CMBlockBufferGetDataLength, CMFormatDescription, CMTimeGetSeconds, CMTimeMakeWithSeconds,
    CMTimeRangeMake,
};
use objc2_core_video::{
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_32BGRA,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_foundation::{NSDictionary, NSNumber, NSString, NSURL};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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

/// Un morceau de la liste de lecture : un fichier, une plage (s) et s'il se répète.
#[derive(Debug, Clone)]
pub struct Segment {
    pub path: PathBuf,
    /// Début de la plage lue (s).
    pub from_s: f64,
    /// Fin de la plage (s) ; `None` = fin du fichier.
    pub to_s: Option<f64>,
    /// Répété jusqu'à l'arrêt (dernier segment seulement).
    pub repeat: bool,
}

/// Liste de lecture d'un calque vidéo.
#[derive(Debug, Clone)]
pub struct Playlist {
    pub segments: Vec<Segment>,
    /// Démarre dès le lancement (sinon, attend `PlayerCtl::start`, au passage à l'antenne).
    pub autostart: bool,
    /// Son du fichier envoyé au bus d'habillage (sinon ignoré : QLab ou un autre lecteur le joue).
    pub audio: bool,
    /// Force le décodage BGRA avec alpha (`None` = détecté d'après le fichier).
    pub alpha: Option<bool>,
}

enum Cmd {
    /// (Re)démarre la liste depuis le début.
    Start,
    /// Arrête si aucun `Start` n'est intervenu depuis la génération indiquée.
    Stop { generation: u64 },
}

/// État partagé entre le thread de lecture et les commandes.
struct Ctl {
    /// Arrêt définitif (Drop du `Player`).
    stop: AtomicBool,
    /// Une commande attend : interrompt la lecture en cours.
    wake: AtomicBool,
    cmds: Mutex<VecDeque<Cmd>>,
    /// Incrémentée à chaque `start` : un arrêt différé (fin de fondu) ne tue pas une lecture
    /// relancée entre-temps.
    generation: AtomicU64,
}

impl Ctl {
    fn interrupted(&self) -> bool {
        self.stop.load(Ordering::Relaxed) || self.wake.load(Ordering::Relaxed)
    }
    fn push(&self, cmd: Cmd) {
        self.cmds.lock().unwrap().push_back(cmd);
        self.wake.store(true, Ordering::Release);
    }
}

/// Télécommande d'un lecteur (clonable, sans effet une fois le lecteur arrêté).
#[derive(Clone)]
pub struct PlayerCtl {
    ctl: Arc<Ctl>,
}

impl PlayerCtl {
    /// (Re)démarre la liste de lecture depuis le début.
    pub fn start(&self) {
        self.ctl.generation.fetch_add(1, Ordering::AcqRel);
        self.ctl.push(Cmd::Start);
    }

    /// Arrête la lecture (et vide l'image) après `after`, sauf si un `start` intervient d'ici
    /// là — pour laisser un fondu sortant se terminer sur l'image en mouvement.
    pub fn stop_after(&self, after: Duration) {
        let generation = self.ctl.generation.load(Ordering::Acquire);
        if after.is_zero() {
            self.ctl.push(Cmd::Stop { generation });
            return;
        }
        let ctl = self.ctl.clone();
        std::thread::spawn(move || {
            std::thread::sleep(after);
            if !ctl.stop.load(Ordering::Relaxed) {
                ctl.push(Cmd::Stop { generation });
            }
        });
    }
}

/// Lecteur d'un calque vidéo d'habillage ; le thread s'arrête et est joint au `Drop`.
pub struct Player {
    ctl: Arc<Ctl>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Player {
    pub fn ctl(&self) -> PlayerCtl {
        PlayerCtl {
            ctl: self.ctl.clone(),
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.ctl.stop.store(true, Ordering::Relaxed);
        self.ctl.wake.store(true, Ordering::Relaxed);
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
    has_alpha: bool,
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
    // Couche alpha : extension `ContainsAlphaChannel` de la description de format (ProRes 4444,
    // HEVC avec alpha…).
    let has_alpha = unsafe { track.formatDescriptions() }
        .firstObject()
        .map(|fd| {
            // SAFETY: les éléments de `formatDescriptions` sont des CMFormatDescription
            // (toll-free bridged avec les objets Objective-C).
            let fd: &CMFormatDescription =
                unsafe { &*(&*fd as *const AnyObject as *const CMFormatDescription) };
            let key = unsafe { kCMFormatDescriptionExtension_ContainsAlphaChannel };
            match unsafe { fd.extension(key) } {
                Some(v) => {
                    if let Some(b) = v.downcast_ref::<CFBoolean>() {
                        b.value()
                    } else if let Some(n) = v.downcast_ref::<CFNumber>() {
                        n.as_i64().unwrap_or(0) != 0
                    } else {
                        false
                    }
                }
                None => false,
            }
        })
        .unwrap_or(false);
    Ok(Probe {
        duration_s: duration,
        width: size.width.max(0.0) as u32,
        height: size.height.max(0.0) as u32,
        fps,
        has_audio,
        has_alpha,
    })
}

/// Réglages de sortie vidéo : NV12 (format natif des décodeurs, zéro conversion) ou BGRA
/// (fichiers avec alpha), compatible Metal, adossé à une IOSurface (importable par le GPU sans
/// copie).
fn video_settings(alpha: bool) -> Retained<NSDictionary<NSString, AnyObject>> {
    let format = NSNumber::numberWithUnsignedInt(if alpha {
        kCVPixelFormatType_32BGRA
    } else {
        kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
    });
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

/// Ouvre et vérifie chaque segment, puis démarre le thread de lecture. L'image va dans `slot`,
/// le son (s'il y en a, que la liste l'autorise et qu'un bus est fourni) dans `audio`. Erreur
/// immédiate si un fichier n'est pas lisible ou si une plage est hors du fichier.
pub fn spawn(
    list: Playlist,
    slot: Arc<FrameSlot>,
    audio: Option<BusInput>,
    sample_rate: u32,
) -> Result<Player> {
    anyhow::ensure!(!list.segments.is_empty(), "liste de lecture vide");
    let mut segments = Vec::with_capacity(list.segments.len());
    let mut alpha = list.alpha.unwrap_or(false);
    for seg in &list.segments {
        let path: PathBuf =
            std::fs::canonicalize(&seg.path).unwrap_or_else(|_| seg.path.to_path_buf());
        let p = probe(&path).with_context(|| format!("vidéo illisible : {}", path.display()))?;
        let to_s = seg.to_s.unwrap_or(p.duration_s);
        anyhow::ensure!(
            seg.from_s >= 0.0 && to_s > seg.from_s && to_s <= p.duration_s + 0.001,
            "plage {:.3} s → {:.3} s hors du fichier {} ({:.3} s)",
            seg.from_s,
            to_s,
            path.display(),
            p.duration_s
        );
        if list.alpha.is_none() && p.has_alpha {
            alpha = true;
        }
        info!(
            "vidéo d'habillage « {} » : {}x{} @ {:.1} fps, {:.1} s{}{}{}{} (AVFoundation)",
            path.display(),
            p.width,
            p.height,
            p.fps,
            p.duration_s,
            if p.has_alpha { ", alpha" } else { "" },
            if p.has_audio && list.audio {
                ", son"
            } else {
                ", muette"
            },
            if seg.from_s > 0.0 || seg.to_s.is_some() {
                format!(", plage {:.2} s → {:.2} s", seg.from_s, to_s)
            } else {
                String::new()
            },
            if seg.repeat { ", en boucle" } else { "" }
        );
        if p.has_audio && list.audio && audio.is_none() {
            debug!(
                "vidéo « {} » : son ignoré (pas de sortie audio)",
                path.display()
            );
        }
        segments.push(Segment {
            path,
            from_s: seg.from_s,
            to_s: Some(to_s),
            repeat: seg.repeat,
        });
    }
    let audio = if list.audio { audio } else { None };
    let ctl = Arc::new(Ctl {
        stop: AtomicBool::new(false),
        wake: AtomicBool::new(false),
        cmds: Mutex::new(VecDeque::new()),
        generation: AtomicU64::new(0),
    });
    if list.autostart {
        ctl.push(Cmd::Start);
    }
    let ctl_t = ctl.clone();
    let name = segments[0]
        .path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let thread = std::thread::Builder::new()
        .name(format!("avf-{name}"))
        .spawn(move || run(segments, alpha, slot, audio, sample_rate, ctl_t))
        .context("thread de lecture vidéo")?;
    Ok(Player {
        ctl,
        thread: Some(thread),
    })
}

/// Lecteur ouvert sur la plage d'un segment, lecture démarrée, **première image déjà décodée** :
/// prêt à afficher sans délai. Gardé « armé » à l'arrêt (segment 0) et pré-ouvert pour le
/// passage suivant pendant la lecture, pour qu'aucune ouverture de fichier ne tombe sur un
/// changement de scène ni sur une couture de boucle.
struct Open {
    reader: Retained<AVAssetReader>,
    vout: Retained<AVAssetReaderTrackOutput>,
    aout: Option<Retained<AVAssetReaderAudioMixOutput>>,
    /// Début de la plage (s) ; les horodatages du fichier y sont ramenés.
    from_s: f64,
    /// Durée de la plage (s).
    duration_s: f64,
    /// Première image (horodatage relatif, image), décodée à l'ouverture.
    first: Option<(f64, Arc<SurfaceFrame>)>,
}

impl Drop for Open {
    fn drop(&mut self) {
        // SAFETY: annuler une lecture en cours libère le décodeur ; sans effet sinon.
        unsafe {
            if self.reader.status() == AVAssetReaderStatus::Reading {
                self.reader.cancelReading();
            }
        }
    }
}

/// Un passage en cours : segment, instant du temps média `from_s`, lecteur, et lecteur
/// pré-ouvert pour le passage suivant.
struct Passage {
    idx: usize,
    origin: Instant,
    open: Open,
    next: Option<Open>,
}

/// Boucle du thread : attend un démarrage, enchaîne les segments, répète le dernier, s'arrête
/// sur commande. Le temps média est continu d'un passage à l'autre. À l'arrêt, le segment 0
/// reste armé et sa première image affichée (visible en preview, départ instantané).
fn run(
    segments: Vec<Segment>,
    alpha: bool,
    slot: Arc<FrameSlot>,
    audio: Option<BusInput>,
    sample_rate: u32,
    ctl: Arc<Ctl>,
) {
    let name = segments[0]
        .path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let with_audio = audio.is_some();
    let open = |idx: usize| -> Option<Open> {
        match open_segment(&segments[idx], alpha, with_audio, sample_rate) {
            Ok(o) => Some(o),
            Err(e) => {
                warn!("vidéo « {name} » : {e:#}");
                None
            }
        }
    };
    // Prochain passage après `idx` : le même s'il se répète, le suivant sinon.
    let next_of = |idx: usize| -> Option<usize> {
        let seg = &segments[idx];
        if seg.repeat {
            Some(idx)
        } else if idx + 1 < segments.len() {
            Some(idx + 1)
        } else {
            None
        }
    };
    // Arme le segment 0 et affiche sa première image.
    let arm = |slot: &FrameSlot| -> Option<Open> {
        let o = open(0)?;
        if let Some((_, f)) = &o.first {
            slot.push(f.clone());
        }
        Some(o)
    };

    let mut armed: Option<Open> = arm(&slot);
    let mut playing: Option<Passage> = None;
    let mut generation = 0u64;
    loop {
        if ctl.stop.load(Ordering::Relaxed) {
            break;
        }
        // Commandes en attente.
        if ctl.wake.swap(false, Ordering::AcqRel) {
            let cmds: Vec<Cmd> = ctl.cmds.lock().unwrap().drain(..).collect();
            for cmd in cmds {
                match cmd {
                    Cmd::Start => {
                        generation = ctl.generation.load(Ordering::Acquire);
                        playing = None;
                        let Some(o) = armed.take().or_else(|| open(0)) else {
                            continue;
                        };
                        debug!("vidéo « {name} » : démarrage");
                        playing = Some(Passage {
                            idx: 0,
                            origin: Instant::now(),
                            open: o,
                            next: None,
                        });
                    }
                    Cmd::Stop { generation: g } => {
                        if g >= generation && playing.is_some() {
                            playing = None;
                            debug!("vidéo « {name} » : arrêt");
                            armed = arm(&slot);
                            if armed.is_none() {
                                slot.clear();
                            }
                        }
                    }
                }
            }
        }
        let Some(mut passage) = playing.take() else {
            // À l'arrêt : on attend une commande.
            if armed.is_none() {
                armed = arm(&slot);
            }
            std::thread::sleep(Duration::from_millis(20));
            continue;
        };
        let idx = passage.idx;
        let next_idx = next_of(idx);
        let open_next = || next_idx.and_then(open);
        match play(
            &mut passage.open,
            &mut passage.next,
            open_next,
            &slot,
            audio.as_ref(),
            passage.origin,
            &ctl,
        ) {
            Ok(Outcome::Interrupted) => {
                // Une commande attend : traitée au tour suivant. La lecture reprend (un
                // `Start` remplace ce passage) ou s'arrête.
                playing = Some(passage);
            }
            Ok(Outcome::Done(duration_s)) => {
                let d = Duration::from_secs_f64(duration_s.max(0.001));
                let mut origin = passage.origin + d;
                // Si la lecture a pris du retard (décodage lent), on resynchronise plutôt que
                // de courir après.
                if origin + Duration::from_millis(100) < Instant::now() {
                    origin = Instant::now();
                }
                match next_idx {
                    Some(n) => {
                        let Some(o) = passage.next.take().or_else(|| open(n)) else {
                            playing = None;
                            continue;
                        };
                        playing = Some(Passage {
                            idx: n,
                            origin,
                            open: o,
                            next: None,
                        });
                    }
                    None => {
                        debug!("vidéo « {name} » : fin de lecture (dernière image conservée)");
                    }
                }
            }
            Err(e) => {
                warn!("vidéo « {name} » : {e:#}");
                if !segments[idx].repeat {
                    continue;
                }
                // Fichier momentanément illisible : nouvel essai dans une seconde.
                for _ in 0..10 {
                    if ctl.interrupted() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                if let Some(o) = open(idx) {
                    playing = Some(Passage {
                        idx,
                        origin: Instant::now(),
                        open: o,
                        next: None,
                    });
                }
            }
        }
    }
}

enum Outcome {
    /// Passage complet : durée média du segment (s).
    Done(f64),
    /// Arrêt ou commande reçue en cours de passage.
    Interrupted,
}

/// Ouvre un lecteur sur la plage d'un segment, démarre la lecture et décode la première image.
fn open_segment(seg: &Segment, alpha: bool, with_audio: bool, sample_rate: u32) -> Result<Open> {
    let path = &seg.path;
    let asset = open_asset(path);
    // SAFETY: statiques AVFoundation ; l'asset est valide.
    let (video_type, audio_type) = unsafe {
        (
            AVMediaTypeVideo.context("AVMediaTypeVideo")?,
            AVMediaTypeAudio.context("AVMediaTypeAudio")?,
        )
    };
    let file_duration_s = unsafe { CMTimeGetSeconds(asset.duration()) };
    let from_s = seg.from_s.max(0.0);
    let to_s = seg.to_s.unwrap_or(file_duration_s).min(file_duration_s);
    let duration_s = (to_s - from_s).max(0.0);
    let vtracks = unsafe { asset.tracksWithMediaType(video_type) };
    let vtrack = vtracks
        .firstObject()
        .ok_or_else(|| anyhow!("aucune piste vidéo"))?;
    let reader = unsafe { AVAssetReader::assetReaderWithAsset_error(&asset) }
        .map_err(|e| anyhow!("AVAssetReader : {}", e.localizedDescription()))?;
    if from_s > 0.0 || seg.to_s.is_some() {
        // SAFETY: plage validée à l'ouverture ; définie avant `startReading`.
        unsafe {
            reader.setTimeRange(CMTimeRangeMake(
                CMTimeMakeWithSeconds(from_s, 600),
                CMTimeMakeWithSeconds(duration_s, 600),
            ));
        }
    }

    // SAFETY: réglages de type correct (NSDictionary<NSString, NSNumber|NSDictionary>).
    let vout = unsafe {
        AVAssetReaderTrackOutput::assetReaderTrackOutputWithTrack_outputSettings(
            &vtrack,
            Some(&video_settings(alpha)),
        )
    };
    unsafe {
        vout.setAlwaysCopiesSampleData(false);
        reader.addOutput(&vout);
    }

    let atracks = unsafe { asset.tracksWithMediaType(audio_type) };
    let aout = if with_audio && atracks.count() > 0 {
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
    } else {
        None
    };

    if !unsafe { reader.startReading() } {
        let why = unsafe { reader.error() }
            .map(|e| e.localizedDescription().to_string())
            .unwrap_or_default();
        return Err(anyhow!("démarrage de la lecture : {why}"));
    }

    // Première image décodée tout de suite : l'affichage au démarrage est instantané.
    let mut first = None;
    while first.is_none() {
        let Some(vsb) = (unsafe { vout.copyNextSampleBuffer() }) else {
            break;
        };
        let vpts = unsafe { CMTimeGetSeconds(vsb.presentation_time_stamp()) } - from_s;
        if let Some(frame) = unsafe { vsb.image_buffer() }.and_then(SurfaceFrame::from_pixel_buffer)
        {
            first = Some((vpts, Arc::new(frame)));
        }
    }
    Ok(Open {
        reader,
        vout,
        aout,
        from_s,
        duration_s,
        first,
    })
}

/// Un passage complet sur un lecteur ouvert. `origin` correspond au temps média `from_s`.
/// Après la première image affichée, pré-ouvre le passage suivant dans `next` (via `open_next`)
/// s'il ne l'est pas déjà.
fn play(
    open: &mut Open,
    next: &mut Option<Open>,
    open_next: impl FnOnce() -> Option<Open>,
    slot: &FrameSlot,
    audio: Option<&BusInput>,
    origin: Instant,
    ctl: &Ctl,
) -> Result<Outcome> {
    let from_s = open.from_s;
    let duration_s = open.duration_s;
    // Le retard d'alignement sur le son n'a de raison d'être qu'avec du son.
    let delay = if open.aout.is_some() {
        AV_DELAY
    } else {
        Duration::ZERO
    };
    let mut open_next = Some(open_next);
    let mut pcm: Vec<u8> = Vec::new();
    let mut audio_pending = None;
    let mut audio_done = open.aout.is_none();
    let mut frames: u64 = 0;
    let mut dropped: u64 = 0;

    // Lecture en pas à pas (une seule file dans AVAssetReader) : pour chaque image, on lit
    // d'abord le son jusqu'à un peu au-delà de son horodatage, puis on attend l'échéance.
    loop {
        if ctl.interrupted() {
            return Ok(Outcome::Interrupted);
        }
        // Image suivante : celle décodée à l'ouverture, puis le flux du lecteur. Horodatages
        // absolus dans le fichier, ramenés au début de la plage.
        let (vpts, frame) = match open.first.take() {
            Some(x) => x,
            None => {
                let Some(vsb) = (unsafe { open.vout.copyNextSampleBuffer() }) else {
                    break;
                };
                let vpts = unsafe { CMTimeGetSeconds(vsb.presentation_time_stamp()) } - from_s;
                let Some(frame) =
                    unsafe { vsb.image_buffer() }.and_then(SurfaceFrame::from_pixel_buffer)
                else {
                    continue;
                };
                (vpts, Arc::new(frame))
            }
        };

        if let (Some(aout), Some(bus)) = (&open.aout, audio) {
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
                let apts = unsafe { CMTimeGetSeconds(asb.presentation_time_stamp()) } - from_s;
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
                            if !push_all(bus, samples, ctl) {
                                return Ok(Outcome::Interrupted);
                            }
                        }
                    }
                }
            }
        }

        let deadline = origin + Duration::from_secs_f64(vpts.max(0.0)) + delay;
        if !wait_until(deadline, ctl) {
            return Ok(Outcome::Interrupted);
        }
        if Instant::now().saturating_duration_since(deadline) > MAX_LATE {
            dropped += 1;
            continue;
        }
        slot.push(frame);
        frames += 1;
        // Première image à l'écran : on pré-ouvre le passage suivant (couture sans attente).
        if next.is_none() {
            if let Some(f) = open_next.take() {
                *next = f();
            }
        }
    }

    // Reste du son après la dernière image.
    if let (Some(aout), Some(bus), false) = (&open.aout, audio, audio_done) {
        let mut rest = audio_pending.take();
        loop {
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
                    if st == 0 && !push_all(bus, bytemuck::cast_slice(&pcm[..len - len % 4]), ctl) {
                        break;
                    }
                }
            }
        }
    }

    let status = unsafe { open.reader.status() };
    if status == AVAssetReaderStatus::Failed {
        let why = unsafe { open.reader.error() }
            .map(|e| e.localizedDescription().to_string())
            .unwrap_or_default();
        return Err(anyhow!("lecture interrompue : {why}"));
    }
    debug!("passage terminé ({frames} images, {dropped} sautées)");
    // Fin du passage : on attend la fin réelle du média (le dernier son est déjà dans le bus).
    let end = origin + Duration::from_secs_f64(duration_s) + delay;
    if !wait_until(end, ctl) {
        return Ok(Outcome::Interrupted);
    }
    Ok(Outcome::Done(duration_s))
}

/// Attend une échéance par petits pas (réactif à l'arrêt et aux commandes). `false` si
/// interrompu.
fn wait_until(deadline: Instant, ctl: &Ctl) -> bool {
    loop {
        if ctl.interrupted() {
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
/// réel ; on est en avance d'au plus `AUDIO_LOOKAHEAD_S`). `false` si interrompu.
fn push_all(bus: &BusInput, mut samples: &[f32], ctl: &Ctl) -> bool {
    while !samples.is_empty() {
        if ctl.interrupted() {
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

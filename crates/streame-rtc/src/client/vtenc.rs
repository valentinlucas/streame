//! Encodage H264 **matériel** par VideoToolbox (côté téléphone), sans libwebrtc.
//!
//! Entrée : des `CVPixelBuffer` de la caméra (NV12, fournis par `AVCaptureVideoDataOutput`
//! dans l'app, ou synthétiques sur le banc). Sortie : des unités d'accès **Annex-B** (SPS/PPS
//! devant chaque image-clé) que le payloader H264 de webrtc-rs découpe en FU-A
//! (`packetization-mode=1`) — exactement ce que le Mac réassemble avec son `SampleBuilder`.
//!
//! Temps réel, pas de réordonnancement d'images (pas de B-frames), délai d'encodage minimal,
//! débit moyen + plafond glissant sur 1 s, image-clé forcée à la demande (PLI/FIR du Mac,
//! nouvelle session, image jetée en aval). [`Scaler`] réduit la résolution avant l'encodeur
//! (`VTPixelTransferSession`, matériel) quand le débit disponible ne justifie plus la pleine
//! définition.
#![allow(deprecated)] // noms C d'Apple (VTCompressionSessionCreate…) pour la lisibilité

use bytes::Bytes;
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFNumberCreate, CFNumberType, CFRetained,
    CFString, CFType,
};
use objc2_core_media::{
    kCMSampleAttachmentKey_NotSync, kCMVideoCodecType_H264,
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex, CMSampleBuffer, CMTime,
};
use objc2_core_video::{
    kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferPixelFormatTypeKey,
    kCVPixelBufferPoolMinimumBufferCountKey, kCVPixelBufferWidthKey, CVPixelBuffer,
    CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
    CVPixelBufferPool, CVPixelBufferPoolCreate, CVPixelBufferPoolCreatePixelBuffer,
};
use objc2_video_toolbox::{
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_DataRateLimits, kVTCompressionPropertyKey_ExpectedFrameRate,
    kVTCompressionPropertyKey_H264EntropyMode, kVTCompressionPropertyKey_MaxFrameDelayCount,
    kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
    kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
    kVTEncodeFrameOptionKey_ForceKeyFrame, kVTH264EntropyMode_CABAC,
    kVTProfileLevel_H264_Baseline_AutoLevel, kVTProfileLevel_H264_High_AutoLevel,
    kVTProfileLevel_H264_Main_AutoLevel, VTCompressionSession, VTCompressionSessionCompleteFrames,
    VTCompressionSessionCreate, VTCompressionSessionEncodeFrame, VTCompressionSessionInvalidate,
    VTCompressionSessionPrepareToEncodeFrames, VTEncodeInfoFlags, VTPixelTransferSession,
    VTPixelTransferSessionCreate, VTPixelTransferSessionInvalidate,
    VTPixelTransferSessionTransferImage, VTSession, VTSessionSetProperty,
};
use std::ffi::{c_char, c_void};
use std::ptr::{null, null_mut, NonNull};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tracing::{debug, info, warn};

/// Intervalle maximal entre deux images-clés (secondes) : filet si un PLI se perd ; le Mac en
/// demande à la demande le reste du temps.
const MAX_KEYFRAME_INTERVAL_S: u32 = 10;

/// Profil H264 négocié (d'après le `profile-level-id` de l'offre du Mac).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Baseline,
    Main,
    High,
}

impl Profile {
    pub fn from_profile_level_id(id: &str) -> Self {
        match id.get(..2).map(|s| s.to_ascii_lowercase()).as_deref() {
            Some("4d") => Profile::Main,
            Some("64") => Profile::High,
            _ => Profile::Baseline,
        }
    }
}

/// Réglages de l'encodeur.
#[derive(Debug, Clone, Copy)]
pub struct EncoderConfig {
    pub fps: u32,
    pub bitrate_bps: u32,
    pub profile: Profile,
}

/// Une unité d'accès encodée (Annex-B).
pub struct EncodedFrame {
    pub data: Bytes,
    pub keyframe: bool,
    /// Instant de capture (horloge monotone locale) : sert d'horodatage d'émission RTP.
    pub captured: Instant,
    /// Horodatage de présentation de la caméra.
    pub pts: Duration,
    pub width: u32,
    pub height: u32,
}

/// Destination des images encodées, remplaçable à chaud (une session WebRTC à la fois).
///
/// Le canal est **borné** ([`Output::QUEUE`]) : si l'écriture RTP ne suit plus (pacer, Wi-Fi),
/// l'image est jetée ici plutôt que d'allonger la latence sans limite, et la prochaine image
/// sera une image-clé (une P jetée casse la chaîne de référence du décodeur).
#[derive(Default)]
pub struct Output {
    tx: Mutex<Option<mpsc::Sender<EncodedFrame>>>,
    need_keyframe: AtomicBool,
    dropped: AtomicU32,
}

impl Output {
    /// Images encodées en attente d'écriture RTP au plus (≈ 100 ms à 30 i/s).
    pub const QUEUE: usize = 3;

    pub fn attach(&self, tx: mpsc::Sender<EncodedFrame>) {
        *self.tx.lock().unwrap() = Some(tx);
    }
    pub fn detach(&self) {
        *self.tx.lock().unwrap() = None;
    }
    pub fn is_attached(&self) -> bool {
        self.tx.lock().map(|t| t.is_some()).unwrap_or(false)
    }
    /// `true` une fois après qu'une image a été jetée faute de place : à encoder en image-clé.
    pub fn take_need_keyframe(&self) -> bool {
        self.need_keyframe.swap(false, Ordering::Relaxed)
    }
    /// Images jetées faute de place en aval depuis le départ.
    pub fn dropped(&self) -> u32 {
        self.dropped.load(Ordering::Relaxed)
    }
    fn send(&self, frame: EncodedFrame) {
        let mut guard = self.tx.lock().unwrap();
        if let Some(tx) = guard.as_ref() {
            match tx.try_send(frame) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                    self.need_keyframe.store(true, Ordering::Relaxed);
                }
                Err(TrySendError::Closed(_)) => *guard = None,
            }
        }
    }
}

/// Compteurs partagés (statistiques).
#[derive(Default)]
pub struct Counters {
    pub frames_out: AtomicU64,
    pub bytes_out: AtomicU64,
    pub keyframes: AtomicU32,
    pub errors: AtomicU32,
    /// Images jetées avant encodage (encodeur en retard) ou par VideoToolbox.
    pub dropped: AtomicU32,
    /// Résolution effectivement encodée (après réduction éventuelle).
    pub width: AtomicU32,
    pub height: AtomicU32,
    /// Sous-alimentations de la sortie audio (re-tamponnages), mises à jour par le pont PCM.
    pub audio_underruns: AtomicU32,
}

/// Réduction de résolution matérielle (`VTPixelTransferSession`) vers un pool de tampons de la
/// taille cible, même format de pixels que la source (NV12).
pub struct Scaler {
    session: CFRetained<VTPixelTransferSession>,
    /// (largeur, hauteur, format) du pool courant.
    pool: Option<(u32, u32, u32, CFRetained<CVPixelBufferPool>)>,
}

unsafe impl Send for Scaler {}

fn untyped(d: &CFDictionary<CFString, CFType>) -> &CFDictionary {
    unsafe { &*(d as *const CFDictionary<CFString, CFType>).cast::<CFDictionary>() }
}

impl Scaler {
    pub fn new() -> Result<Self, String> {
        let mut out: *mut VTPixelTransferSession = null_mut();
        let status = unsafe { VTPixelTransferSessionCreate(None, NonNull::from(&mut out)) };
        let ptr = NonNull::new(out)
            .filter(|_| status == 0)
            .ok_or_else(|| format!("VTPixelTransferSessionCreate : {status}"))?;
        Ok(Self { session: unsafe { CFRetained::from_raw(ptr) }, pool: None })
    }

    fn create_pool(width: u32, height: u32, format: u32) -> Result<CFRetained<CVPixelBufferPool>, String> {
        let w = cf_i32(width as i32);
        let h = cf_i32(height as i32);
        let f = cf_i32(format as i32);
        let io = CFDictionary::<CFString, CFType>::from_slices(&[], &[]);
        let io_ref: &CFType = unsafe { &*(&*io as *const CFDictionary<CFString, CFType>).cast::<CFType>() };
        let attrs = unsafe {
            CFDictionary::<CFString, CFType>::from_slices(
                &[
                    kCVPixelBufferWidthKey,
                    kCVPixelBufferHeightKey,
                    kCVPixelBufferPixelFormatTypeKey,
                    kCVPixelBufferIOSurfacePropertiesKey,
                ],
                &[&*w, &*h, &*f, io_ref],
            )
        };
        let min = cf_i32(3);
        let pool_attrs = unsafe {
            CFDictionary::<CFString, CFType>::from_slices(&[kCVPixelBufferPoolMinimumBufferCountKey], &[&*min])
        };
        let mut out: *mut CVPixelBufferPool = null_mut();
        let status = unsafe {
            CVPixelBufferPoolCreate(None, Some(untyped(&pool_attrs)), Some(untyped(&attrs)), NonNull::from(&mut out))
        };
        NonNull::new(out)
            .filter(|_| status == 0)
            .map(|p| unsafe { CFRetained::from_raw(p) })
            .ok_or_else(|| format!("CVPixelBufferPoolCreate {width}x{height} : {status}"))
    }

    /// Copie `src` réduite à `width`×`height` dans un tampon du pool.
    pub fn scale(&mut self, src: &CVPixelBuffer, width: u32, height: u32) -> Result<CFRetained<CVPixelBuffer>, String> {
        let format = CVPixelBufferGetPixelFormatType(src);
        if self.pool.as_ref().map_or(true, |(w, h, f, _)| (*w, *h, *f) != (width, height, format)) {
            self.pool = Some((width, height, format, Self::create_pool(width, height, format)?));
        }
        let pool = &self.pool.as_ref().unwrap().3;
        let mut out: *mut CVPixelBuffer = null_mut();
        let status = unsafe { CVPixelBufferPoolCreatePixelBuffer(None, pool, NonNull::from(&mut out)) };
        let dst = NonNull::new(out)
            .filter(|_| status == 0)
            .map(|p| unsafe { CFRetained::from_raw(p) })
            .ok_or_else(|| format!("CVPixelBufferPoolCreatePixelBuffer : {status}"))?;
        let status = unsafe { VTPixelTransferSessionTransferImage(&self.session, src, &dst) };
        if status != 0 {
            return Err(format!("VTPixelTransferSessionTransferImage : {status}"));
        }
        Ok(dst)
    }
}

impl Drop for Scaler {
    fn drop(&mut self) {
        unsafe { VTPixelTransferSessionInvalidate(&self.session) };
    }
}

/// Dimensions cibles pour plafonner la hauteur à `max_height` en conservant le rapport
/// d'image (largeur paire). `None` si la source est déjà assez petite.
pub fn scaled_dims(width: u32, height: u32, max_height: u32) -> Option<(u32, u32)> {
    if max_height == 0 || height <= max_height {
        return None;
    }
    let w = (width as u64 * max_height as u64 / height as u64) as u32 & !1;
    Some((w.max(2), max_height & !1))
}

/// Partagé avec le callback VideoToolbox (thread de l'encodeur).
struct Sink {
    output: Arc<Output>,
    counters: Arc<Counters>,
}

/// Métadonnées d'une image en vol, passées en `sourceFrameRefCon` (boîte reprise au callback).
struct FrameMeta {
    captured: Instant,
    pts: Duration,
    width: u32,
    height: u32,
}

pub struct H264Encoder {
    session: Option<CFRetained<VTCompressionSession>>,
    cfg: EncoderConfig,
    width: u32,
    height: u32,
    sink: Arc<Sink>,
    frames_in: u64,
}

unsafe impl Send for H264Encoder {}

fn cf_i32(v: i32) -> CFRetained<CFNumber> {
    unsafe { CFNumberCreate(None, CFNumberType::SInt32Type, &v as *const i32 as *const c_void) }
        .expect("CFNumber")
}

fn cf_f64(v: f64) -> CFRetained<CFNumber> {
    unsafe { CFNumberCreate(None, CFNumberType::Float64Type, &v as *const f64 as *const c_void) }
        .expect("CFNumber")
}

/// Callback VideoToolbox : convertit le `CMSampleBuffer` (AVCC) en Annex-B et l'émet.
unsafe extern "C-unwind" fn on_encoded(
    refcon: *mut c_void,
    source: *mut c_void,
    status: i32,
    flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    // SAFETY: `refcon` pointe sur le `Sink` détenu par l'encodeur, qui invalide la session
    // (plus aucun callback) avant de le libérer.
    let sink = unsafe { &*(refcon as *const Sink) };
    // SAFETY: `source` est la boîte `FrameMeta` créée dans `encode` pour cette image ; VT nous
    // la rend exactement une fois (succès, erreur ou image sautée).
    let meta = NonNull::new(source as *mut FrameMeta).map(|p| unsafe { Box::from_raw(p.as_ptr()) });
    if status != 0 {
        sink.counters.errors.fetch_add(1, Ordering::Relaxed);
        return;
    }
    if flags.contains(VTEncodeInfoFlags::FrameDropped) || sample.is_null() {
        sink.counters.dropped.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let Some(meta) = meta else { return };
    // SAFETY: pointeur valide pour la durée du callback.
    let sample = unsafe { &*sample };
    match annexb_from_sample(sample) {
        Some((data, keyframe)) => {
            sink.counters.frames_out.fetch_add(1, Ordering::Relaxed);
            sink.counters.bytes_out.fetch_add(data.len() as u64, Ordering::Relaxed);
            if keyframe {
                sink.counters.keyframes.fetch_add(1, Ordering::Relaxed);
            }
            sink.output.send(EncodedFrame {
                data: Bytes::from(data),
                keyframe,
                captured: meta.captured,
                pts: meta.pts,
                width: meta.width,
                height: meta.height,
            });
        }
        None => {
            sink.counters.errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// AVCC (NALUs préfixées de leur longueur) + jeux de paramètres → Annex-B ; `true` si image-clé.
fn annexb_from_sample(sample: &CMSampleBuffer) -> Option<(Vec<u8>, bool)> {
    // Image-clé : l'attachement `NotSync` est absent (ou faux).
    let keyframe = unsafe {
        match sample.sample_attachments_array(false) {
            Some(arr) if arr.count() > 0 => {
                let dict = arr.value_at_index(0) as *const CFDictionary;
                if dict.is_null() {
                    true
                } else {
                    let key = kCMSampleAttachmentKey_NotSync as *const CFString as *const c_void;
                    let v = (*dict).value(key);
                    if v.is_null() {
                        true
                    } else {
                        let b = &*(v as *const CFBoolean);
                        !b.as_bool()
                    }
                }
            }
            _ => true,
        }
    };
    let mut out: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut header_len: i32 = 4;
    if keyframe {
        let fd = unsafe { sample.format_description() }?;
        let mut count: usize = 0;
        let mut i = 0usize;
        loop {
            let mut ptr: *const u8 = null();
            let mut size: usize = 0;
            let status = unsafe {
                CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                    &fd,
                    i,
                    &mut ptr,
                    &mut size,
                    &mut count,
                    &mut header_len,
                )
            };
            if status != 0 || ptr.is_null() {
                break;
            }
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(unsafe { std::slice::from_raw_parts(ptr, size) });
            i += 1;
            if i >= count {
                break;
            }
        }
    }
    let block = unsafe { sample.data_buffer() }?;
    let mut total: usize = 0;
    let mut at: usize = 0;
    let mut ptr: *mut c_char = null_mut();
    let status = unsafe { block.data_pointer(0, &mut at, &mut total, &mut ptr) };
    if status != 0 || total == 0 {
        return None;
    }
    let contiguous: Vec<u8>;
    let avcc: &[u8] = if at == total && !ptr.is_null() {
        unsafe { std::slice::from_raw_parts(ptr as *const u8, total) }
    } else {
        let mut v = vec![0u8; total];
        let st = unsafe { block.copy_data_bytes(0, total, NonNull::from(&mut v[0]).cast::<c_void>()) };
        if st != 0 {
            return None;
        }
        contiguous = v;
        &contiguous
    };
    let hl = header_len.clamp(1, 4) as usize;
    let mut i = 0usize;
    while i + hl <= avcc.len() {
        let mut len = 0usize;
        for b in &avcc[i..i + hl] {
            len = (len << 8) | *b as usize;
        }
        i += hl;
        if len == 0 || i + len > avcc.len() {
            break;
        }
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(&avcc[i..i + len]);
        i += len;
    }
    Some((out, keyframe))
}

impl H264Encoder {
    pub fn new(cfg: EncoderConfig, output: Arc<Output>, counters: Arc<Counters>) -> Self {
        Self {
            session: None,
            cfg,
            width: 0,
            height: 0,
            sink: Arc::new(Sink { output, counters }),
            frames_in: 0,
        }
    }

    pub fn bitrate(&self) -> u32 {
        self.cfg.bitrate_bps
    }

    pub fn frames_submitted(&self) -> u64 {
        self.frames_in
    }

    fn destroy_session(&mut self) {
        if let Some(s) = self.session.take() {
            unsafe {
                let _ = VTCompressionSessionCompleteFrames(&s, CMTime::new(0, 1));
                VTCompressionSessionInvalidate(&s);
            }
        }
    }

    fn set_property(&self, key: &CFString, value: &CFType) -> Result<(), String> {
        let Some(session) = self.session.as_ref() else {
            return Err("session absente".into());
        };
        // VTCompressionSession « est un » VTSession (même représentation CF).
        let vts: &VTSession = unsafe { &*CFRetained::as_ptr(session).cast::<VTSession>().as_ptr() };
        let status = unsafe { VTSessionSetProperty(vts, key, Some(value)) };
        if status != 0 {
            return Err(format!("VTSessionSetProperty({key}) : {status}"));
        }
        Ok(())
    }

    fn apply_bitrate(&self) -> Result<(), String> {
        let bps = self.cfg.bitrate_bps.max(100_000);
        unsafe {
            self.set_property(kVTCompressionPropertyKey_AverageBitRate, &cf_i32(bps as i32))?;
            // Plafond glissant : pas plus de 1,25 × le débit moyen sur une fenêtre d'une seconde
            // (borne les rafales d'images-clés que le Wi-Fi encaisse mal).
            let bytes_per_s = cf_i32((bps as f64 * 1.25 / 8.0) as i32);
            let window = cf_f64(1.0);
            let limits: CFRetained<CFArray<CFType>> = CFArray::from_objects(&[&*bytes_per_s, &*window]);
            self.set_property(kVTCompressionPropertyKey_DataRateLimits, &limits)?;
        }
        Ok(())
    }

    fn create_session(&mut self, width: u32, height: u32) -> Result<(), String> {
        self.destroy_session();
        let mut out: *mut VTCompressionSession = null_mut();
        let status = unsafe {
            VTCompressionSessionCreate(
                None,
                width as i32,
                height as i32,
                kCMVideoCodecType_H264,
                None,
                None,
                None,
                Some(on_encoded),
                Arc::as_ptr(&self.sink) as *mut c_void,
                NonNull::from(&mut out),
            )
        };
        let ptr = NonNull::new(out)
            .filter(|_| status == 0)
            .ok_or_else(|| format!("VTCompressionSessionCreate : {status}"))?;
        self.session = Some(unsafe { CFRetained::from_raw(ptr) });
        self.width = width;
        self.height = height;
        self.sink.counters.width.store(width, Ordering::Relaxed);
        self.sink.counters.height.store(height, Ordering::Relaxed);
        unsafe {
            let yes = CFBoolean::new(true);
            let no = CFBoolean::new(false);
            self.set_property(kVTCompressionPropertyKey_RealTime, yes)?;
            self.set_property(kVTCompressionPropertyKey_AllowFrameReordering, no)?;
            let (profile, cabac) = match self.cfg.profile {
                Profile::Baseline => (kVTProfileLevel_H264_Baseline_AutoLevel, false),
                Profile::Main => (kVTProfileLevel_H264_Main_AutoLevel, true),
                Profile::High => (kVTProfileLevel_H264_High_AutoLevel, true),
            };
            self.set_property(kVTCompressionPropertyKey_ProfileLevel, profile)?;
            if cabac {
                // Facultatif selon l'encodeur : une erreur n'est pas bloquante.
                let _ = self.set_property(kVTCompressionPropertyKey_H264EntropyMode, kVTH264EntropyMode_CABAC);
            }
            let fps = self.cfg.fps.max(1);
            self.set_property(kVTCompressionPropertyKey_ExpectedFrameRate, &cf_f64(fps as f64))?;
            // Latence : au plus une image retenue dans l'encodeur, vitesse avant qualité.
            // Facultatifs selon l'encodeur : une erreur n'est pas bloquante.
            let _ = self.set_property(kVTCompressionPropertyKey_MaxFrameDelayCount, &cf_i32(1));
            let _ = self.set_property(kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality, yes);
            // Image-clé périodique : filet seulement (le Mac demande par PLI ce qu'il lui faut).
            self.set_property(
                kVTCompressionPropertyKey_MaxKeyFrameInterval,
                &cf_i32((fps * MAX_KEYFRAME_INTERVAL_S) as i32),
            )?;
            self.set_property(
                kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
                &cf_f64(MAX_KEYFRAME_INTERVAL_S as f64),
            )?;
        }
        self.apply_bitrate()?;
        let status = unsafe { VTCompressionSessionPrepareToEncodeFrames(self.session.as_ref().unwrap()) };
        if status != 0 {
            return Err(format!("VTCompressionSessionPrepareToEncodeFrames : {status}"));
        }
        info!(
            "VideoToolbox : encodeur H264 {width}x{height} @ {} i/s, {:?}, {} kb/s",
            self.cfg.fps,
            self.cfg.profile,
            self.cfg.bitrate_bps / 1000
        );
        Ok(())
    }

    /// Change le débit cible à chaud (sans recréer la session).
    pub fn set_bitrate(&mut self, bps: u32) {
        if bps == self.cfg.bitrate_bps {
            return;
        }
        self.cfg.bitrate_bps = bps;
        if self.session.is_some() {
            if let Err(e) = self.apply_bitrate() {
                warn!("VideoToolbox : débit : {e}");
            } else {
                debug!("VideoToolbox : débit cible {} kb/s", bps / 1000);
            }
        }
    }

    /// Soumet une image. `pts` = horodatage de présentation de la caméra ; `force_keyframe`
    /// demande une IDR (PLI/FIR reçu, nouvelle session WebRTC).
    pub fn encode(
        &mut self,
        pixel_buffer: &CVPixelBuffer,
        pts: Duration,
        captured: Instant,
        force_keyframe: bool,
    ) -> Result<(), String> {
        let width = CVPixelBufferGetWidth(pixel_buffer) as u32;
        let height = CVPixelBufferGetHeight(pixel_buffer) as u32;
        if width == 0 || height == 0 {
            return Err("image vide".into());
        }
        if self.session.is_none() || width != self.width || height != self.height {
            self.create_session(width, height)?;
        }
        let session = self.session.as_ref().unwrap();
        let props: Option<CFRetained<CFDictionary<CFString, CFType>>> = force_keyframe.then(|| {
            let key: &CFString = unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame };
            let yes: &CFType = CFBoolean::new(true);
            CFDictionary::from_slices(&[key], &[yes])
        });
        let props_ref: Option<&CFDictionary> = props
            .as_ref()
            .map(|d| unsafe { &*CFRetained::as_ptr(d).cast::<CFDictionary>().as_ptr() });
        let meta = Box::into_raw(Box::new(FrameMeta { captured, pts, width, height }));
        let pts_cm = unsafe { CMTime::new(pts.as_micros() as i64, 1_000_000) };
        let dur_cm = unsafe { CMTime::new(1_000_000 / self.cfg.fps.max(1) as i64, 1_000_000) };
        let mut flags = VTEncodeInfoFlags::empty();
        let status = unsafe {
            VTCompressionSessionEncodeFrame(
                session,
                pixel_buffer,
                pts_cm,
                dur_cm,
                props_ref,
                meta as *mut c_void,
                &mut flags,
            )
        };
        if status != 0 {
            // Pas de callback en cas d'échec synchrone : on reprend la boîte.
            drop(unsafe { Box::from_raw(meta) });
            // Session probablement invalide (retour d'arrière-plan sur iOS…) : on la recrée
            // à la prochaine image.
            self.destroy_session();
            return Err(format!("VTCompressionSessionEncodeFrame : {status}"));
        }
        self.frames_in += 1;
        Ok(())
    }
}

impl Drop for H264Encoder {
    fn drop(&mut self) {
        self.destroy_session();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profil_depuis_profile_level_id() {
        assert_eq!(Profile::from_profile_level_id("42e01f"), Profile::Baseline);
        assert_eq!(Profile::from_profile_level_id("4d001f"), Profile::Main);
        assert_eq!(Profile::from_profile_level_id("640C1F"), Profile::High);
        assert_eq!(Profile::from_profile_level_id(""), Profile::Baseline);
    }

    #[test]
    fn dimensions_reduites() {
        assert_eq!(scaled_dims(1920, 1080, 720), Some((1280, 720)));
        assert_eq!(scaled_dims(1920, 1080, 540), Some((960, 540)));
        assert_eq!(scaled_dims(1920, 1080, 360), Some((640, 360)));
        assert_eq!(scaled_dims(640, 480, 360), Some((480, 360)));
        assert_eq!(scaled_dims(1280, 720, 720), None);
        assert_eq!(scaled_dims(1280, 720, 0), None);
    }

    #[test]
    fn sortie_bornee_jette_et_demande_une_image_cle() {
        let out = Output::default();
        let (tx, mut rx) = mpsc::channel(Output::QUEUE);
        out.attach(tx);
        let frame = || EncodedFrame {
            data: Bytes::new(),
            keyframe: false,
            captured: Instant::now(),
            pts: Duration::ZERO,
            width: 2,
            height: 2,
        };
        for _ in 0..Output::QUEUE + 2 {
            out.send(frame());
        }
        assert_eq!(out.dropped(), 2);
        assert!(out.take_need_keyframe());
        assert!(!out.take_need_keyframe());
        assert!(rx.try_recv().is_ok());
    }
}

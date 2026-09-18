//! Décodage H264 **matériel** par VideoToolbox, sans GStreamer.
//!
//! Entrée : unités d'accès Annex-B (sorties du dépaquetiseur RTP de webrtc-rs). Sortie : des
//! `CVPixelBuffer` NV12 adossés à une IOSurface, livrés de façon asynchrone par le callback de
//! VideoToolbox — le rendu les importe comme textures Metal sans copie (voir `render.rs`).
//!
//! Résilience : la session est (re)créée dès que SPS/PPS changent ; après une perte ou une
//! erreur de décodage on jette tout jusqu'à la prochaine image-clé (IDR) et on signale à
//! l'appelant qu'il faut en demander une (PLI) — bref gel plutôt qu'image corrompue.
#![allow(deprecated)] // on garde les noms C d'Apple (VTDecompressionSessionCreate…) pour la lisibilité

use crate::frame::SurfaceFrame;
use objc2_core_foundation::{
    CFBoolean, CFDictionary, CFNumberCreate, CFNumberType, CFRetained, CFString, CFType,
};
use objc2_core_media::{
    kCMBlockBufferAssureMemoryNowFlag, kCMTimeInvalid, CMBlockBuffer,
    CMBlockBufferCreateWithMemoryBlock, CMBlockBufferReplaceDataBytes, CMFormatDescription,
    CMSampleBuffer, CMSampleBufferCreateReady, CMSampleTimingInfo, CMTime,
    CMVideoFormatDescriptionCreateFromH264ParameterSets,
};
use objc2_core_video::{
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
    CVImageBuffer, CVPixelBuffer,
};
use objc2_video_toolbox::{
    VTDecodeFrameFlags, VTDecodeInfoFlags, VTDecompressionOutputCallbackRecord,
    VTDecompressionSession, VTDecompressionSessionCreate, VTDecompressionSessionDecodeFrame,
    VTDecompressionSessionInvalidate, VTDecompressionSessionWaitForAsynchronousFrames,
};
use std::ffi::c_void;
use std::ptr::{null, null_mut, NonNull};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;

/// Image décodée, avec son instant d'arrivée (pour le retard de lip-sync côté présentation).
pub type Decoded = (Instant, Arc<SurfaceFrame>);

/// Résultat de la soumission d'une unité d'accès.
pub enum Outcome {
    /// Soumise au décodeur (la sortie arrive par le canal, de façon asynchrone).
    Ok,
    /// Jetée : il faut une image-clé (pas encore de SPS/PPS, ou perte/erreur en cours).
    NeedKeyframe,
    /// Erreur VideoToolbox ; une image-clé est aussi nécessaire.
    Error(String),
}

/// Destination des images décodées + compteur d'erreurs asynchrones (callback → thread appelant).
struct Sink {
    tx: mpsc::UnboundedSender<Decoded>,
    errors: AtomicU32,
    /// Images sautées par le décodeur (statut OK sans image) : simple compteur.
    dropped: AtomicU32,
}

pub struct H264Decoder {
    session: Option<CFRetained<VTDecompressionSession>>,
    format: Option<CFRetained<CMFormatDescription>>,
    sps: Vec<u8>,
    pps: Vec<u8>,
    /// Attendre une image-clé avant de soumettre quoi que ce soit.
    need_idr: bool,
    sink: Arc<Sink>,
    frames_in: u64,
}

unsafe impl Send for H264Decoder {}

/// Callback VideoToolbox (thread du décodeur) : enveloppe le `CVPixelBuffer` et l'envoie.
unsafe extern "C-unwind" fn on_frame(
    refcon: *mut c_void,
    _source_frame: *mut c_void,
    status: i32,
    _flags: VTDecodeInfoFlags,
    image: *mut CVImageBuffer,
    _pts: CMTime,
    _duration: CMTime,
) {
    // SAFETY: `refcon` pointe sur le `Sink` détenu par le décodeur, qui invalide la session
    // (plus aucun callback) avant de le libérer.
    let sink = unsafe { &*(refcon as *const Sink) };
    if status != 0 {
        sink.errors.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let Some(image) = NonNull::new(image) else {
        // Statut OK sans image : le décodeur a sauté l'image (kVTDecodeInfo_FrameDropped). Ce
        // n'est pas une erreur de flux ; si une référence manque ensuite, la suivante échouera
        // avec un statut, et c'est elle qui déclenchera la demande d'image-clé.
        sink.dropped.fetch_add(1, Ordering::Relaxed);
        return;
    };
    // SAFETY: le pointeur est un CVPixelBuffer valide pour la durée du callback ; on le retient.
    let pixel_buffer: CFRetained<CVPixelBuffer> = unsafe { CFRetained::retain(image) };
    match SurfaceFrame::from_pixel_buffer(pixel_buffer) {
        Some(frame) => {
            let _ = sink.tx.send((Instant::now(), Arc::new(frame)));
        }
        None => {
            sink.errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Découpe un flux Annex-B en NALUs (sans les codes de démarrage).
fn nalus(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut start: Option<usize> = None;
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if let Some(s) = start {
                let mut end = i;
                if end > s && data[end - 1] == 0 {
                    end -= 1; // code à 4 octets (00 00 00 01)
                }
                if end > s {
                    out.push(&data[s..end]);
                }
            }
            i += 3;
            start = Some(i);
        } else {
            i += 1;
        }
    }
    if let Some(s) = start {
        if s < data.len() {
            out.push(&data[s..]);
        }
    }
    out
}

/// Attributs des tampons de sortie : NV12 (« 420v »), compatibles Metal, adossés à une IOSurface.
fn destination_attributes() -> CFRetained<CFDictionary<CFString, CFType>> {
    let pixel_format: i32 = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange as i32;
    let number = unsafe {
        CFNumberCreate(
            None,
            CFNumberType::SInt32Type,
            &pixel_format as *const i32 as *const c_void,
        )
    }
    .expect("CFNumber");
    let yes = CFBoolean::new(true);
    let surface_props: CFRetained<CFDictionary<CFString, CFType>> =
        CFDictionary::from_slices(&[], &[]);
    // SAFETY: statiques exportées par CoreVideo.
    let keys: [&CFString; 3] = unsafe {
        [
            kCVPixelBufferPixelFormatTypeKey,
            kCVPixelBufferMetalCompatibilityKey,
            kCVPixelBufferIOSurfacePropertiesKey,
        ]
    };
    let values: [&CFType; 3] = [&number, yes, &surface_props];
    CFDictionary::from_slices(&keys, &values)
}

fn format_description(sps: &[u8], pps: &[u8]) -> Result<CFRetained<CMFormatDescription>, String> {
    if sps.is_empty() || pps.is_empty() {
        return Err("SPS/PPS absents".into());
    }
    let mut pointers = [NonNull::from(&sps[0]), NonNull::from(&pps[0])];
    let mut sizes = [sps.len(), pps.len()];
    let mut out: *const CMFormatDescription = null();
    let status = unsafe {
        CMVideoFormatDescriptionCreateFromH264ParameterSets(
            None,
            2,
            NonNull::from(&mut pointers[0]),
            NonNull::from(&mut sizes[0]),
            4,
            NonNull::from(&mut out),
        )
    };
    let ptr = NonNull::new(out as *mut CMFormatDescription)
        .filter(|_| status == 0)
        .ok_or_else(|| format!("CMVideoFormatDescriptionCreateFromH264ParameterSets : {status}"))?;
    // SAFETY: la fonction « Create » renvoie une référence +1 qui nous appartient.
    Ok(unsafe { CFRetained::from_raw(ptr) })
}

impl H264Decoder {
    pub fn new(tx: mpsc::UnboundedSender<Decoded>) -> Self {
        Self {
            session: None,
            format: None,
            sps: Vec::new(),
            pps: Vec::new(),
            need_idr: true,
            sink: Arc::new(Sink {
                tx,
                errors: AtomicU32::new(0),
                dropped: AtomicU32::new(0),
            }),
            frames_in: 0,
        }
    }

    /// Une perte de paquets a été constatée en amont : on attend la prochaine image-clé.
    pub fn mark_loss(&mut self) {
        self.need_idr = true;
    }

    pub fn frames_submitted(&self) -> u64 {
        self.frames_in
    }

    fn destroy_session(&mut self) {
        if let Some(s) = self.session.take() {
            unsafe {
                VTDecompressionSessionWaitForAsynchronousFrames(&s);
                VTDecompressionSessionInvalidate(&s);
            }
        }
        self.format = None;
    }

    fn create_session(&mut self) -> Result<(), String> {
        self.destroy_session();
        let format = format_description(&self.sps, &self.pps)?;
        let attrs = destination_attributes();
        // Le dictionnaire typé a la même représentation que le dictionnaire opaque attendu.
        let attrs_ref: &CFDictionary = unsafe { &*CFRetained::as_ptr(&attrs).cast::<CFDictionary>().as_ptr() };
        let record = VTDecompressionOutputCallbackRecord {
            decompressionOutputCallback: Some(on_frame),
            decompressionOutputRefCon: Arc::as_ptr(&self.sink) as *mut c_void,
        };
        let mut out: *mut VTDecompressionSession = null_mut();
        let status = unsafe {
            VTDecompressionSessionCreate(
                None,
                &format,
                None,
                Some(attrs_ref),
                &record,
                NonNull::from(&mut out),
            )
        };
        let ptr = NonNull::new(out)
            .filter(|_| status == 0)
            .ok_or_else(|| format!("VTDecompressionSessionCreate : {status}"))?;
        self.session = Some(unsafe { CFRetained::from_raw(ptr) });
        self.format = Some(format);
        self.need_idr = true;
        // Les images encore en vol dans l'ancienne session se terminent en erreur pendant
        // `destroy_session` : sans rapport avec la nouvelle, dont on exige de toute façon un IDR.
        self.sink.errors.store(0, Ordering::Relaxed);
        tracing::debug!("VideoToolbox : session (re)créée (SPS/PPS)");
        Ok(())
    }

    /// Soumet une unité d'accès (AVCC : NALUs préfixées de leur longueur) au décodeur.
    fn decode_au(&mut self, avcc: &[u8], rtp_ts: u32) -> Result<(), String> {
        let (Some(session), Some(format)) = (self.session.as_ref(), self.format.as_ref()) else {
            return Err("session absente".into());
        };
        let mut block: *mut CMBlockBuffer = null_mut();
        let status = unsafe {
            CMBlockBufferCreateWithMemoryBlock(
                None,
                null_mut(),
                avcc.len(),
                None,
                null(),
                0,
                avcc.len(),
                kCMBlockBufferAssureMemoryNowFlag,
                NonNull::from(&mut block),
            )
        };
        let block = NonNull::new(block)
            .filter(|_| status == 0)
            .map(|p| unsafe { CFRetained::from_raw(p) })
            .ok_or_else(|| format!("CMBlockBufferCreateWithMemoryBlock : {status}"))?;
        let status = unsafe {
            CMBlockBufferReplaceDataBytes(
                NonNull::from(&avcc[0]).cast::<c_void>(),
                &block,
                0,
                avcc.len(),
            )
        };
        if status != 0 {
            return Err(format!("CMBlockBufferReplaceDataBytes : {status}"));
        }
        let timing = CMSampleTimingInfo {
            duration: unsafe { kCMTimeInvalid },
            presentationTimeStamp: unsafe { CMTime::new(rtp_ts as i64, 90_000) },
            decodeTimeStamp: unsafe { kCMTimeInvalid },
        };
        let sizes = [avcc.len()];
        let mut sample: *mut CMSampleBuffer = null_mut();
        let status = unsafe {
            CMSampleBufferCreateReady(
                None,
                Some(&block),
                Some(format),
                1,
                1,
                &timing,
                1,
                sizes.as_ptr(),
                NonNull::from(&mut sample),
            )
        };
        let sample = NonNull::new(sample)
            .filter(|_| status == 0)
            .map(|p| unsafe { CFRetained::from_raw(p) })
            .ok_or_else(|| format!("CMSampleBufferCreateReady : {status}"))?;
        let mut info = VTDecodeInfoFlags::empty();
        // Bit 0 : décompression asynchrone (pipeline du décodeur, latence minimale) ; bit 2 :
        // lecture temps réel (pas de mise en attente pour lisser le débit).
        let flags = VTDecodeFrameFlags::from_bits_retain(1 | 4);
        let status = unsafe {
            VTDecompressionSessionDecodeFrame(session, &sample, flags, null_mut(), &mut info)
        };
        if status != 0 {
            return Err(format!("VTDecompressionSessionDecodeFrame : {status}"));
        }
        self.frames_in += 1;
        Ok(())
    }

    /// Traite une unité d'accès Annex-B complète (horodatage RTP à 90 kHz).
    pub fn decode(&mut self, annexb: &[u8], rtp_ts: u32) -> Outcome {
        let errors = self.sink.errors.swap(0, Ordering::Relaxed);
        if errors > 0 {
            // Une image précédente a échoué au décodage : on repart d'une image-clé.
            tracing::debug!("VideoToolbox : {errors} image(s) en erreur, image-clé requise");
            self.need_idr = true;
        }
        let mut avcc = Vec::with_capacity(annexb.len() + 32);
        let mut has_idr = false;
        let mut new_params = false;
        for nal in nalus(annexb) {
            match nal[0] & 0x1f {
                7 => {
                    if self.sps != nal {
                        self.sps = nal.to_vec();
                        new_params = true;
                    }
                }
                8 => {
                    if self.pps != nal {
                        self.pps = nal.to_vec();
                        new_params = true;
                    }
                }
                9 | 12 => {} // délimiteur d'unité d'accès, bourrage : inutiles
                t => {
                    if t == 5 {
                        has_idr = true;
                    }
                    avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                    avcc.extend_from_slice(nal);
                }
            }
        }
        if (new_params || self.session.is_none()) && !self.sps.is_empty() && !self.pps.is_empty() {
            if let Err(e) = self.create_session() {
                return Outcome::Error(e);
            }
        }
        if self.session.is_none() {
            return Outcome::NeedKeyframe;
        }
        if has_idr {
            self.need_idr = false;
        }
        if self.need_idr {
            return Outcome::NeedKeyframe;
        }
        if avcc.is_empty() {
            return Outcome::Ok;
        }
        match self.decode_au(&avcc, rtp_ts) {
            Ok(()) => Outcome::Ok,
            Err(e) => {
                self.need_idr = true;
                Outcome::Error(e)
            }
        }
    }
}

impl Drop for H264Decoder {
    fn drop(&mut self) {
        // Plus aucun callback après l'invalidation : le `Sink` peut alors être libéré.
        self.destroy_session();
    }
}

#[cfg(test)]
mod tests {
    use super::nalus;

    #[test]
    fn decoupe_annexb() {
        let data = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4, 5, 6];
        let n = nalus(&data);
        assert_eq!(n.len(), 3);
        assert_eq!(n[0], &[0x67, 1, 2]);
        assert_eq!(n[1], &[0x68, 3]);
        assert_eq!(n[2], &[0x65, 4, 5, 6]);
    }
}

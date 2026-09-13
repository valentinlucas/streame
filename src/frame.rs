//! Échange d'images entre les décodeurs et le rendu : chaque source (téléphone, fichier vidéo)
//! possède un emplacement contenant la dernière image reçue.
//!
//! Deux natures d'image :
//! - [`Frame::Surface`] : image décodée par VideoToolbox/AVFoundation, adossée à une **IOSurface**
//!   (mémoire partagée GPU). Le rendu l'importe directement comme texture Metal — **zéro copie**.
//! - [`Frame::Gst`] : image GStreamer en mémoire système (chemin historique, copiée vers le GPU).

use gst_video::prelude::*;
use objc2_core_foundation::CFRetained;
use objc2_core_video::{
    kCVPixelFormatType_32BGRA, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, CVPixelBuffer, CVPixelBufferGetHeight,
    CVPixelBufferGetIOSurface, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
};
use objc2_io_surface::IOSurfaceRef;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Image décodée en mémoire GPU (IOSurface). Le `CVPixelBuffer` est conservé tant que l'image
/// est référencée (par l'emplacement ou par une texture importée) : sinon VideoToolbox
/// recyclerait le tampon sous les pieds du GPU.
pub struct SurfaceFrame {
    pub pixel_buffer: CFRetained<CVPixelBuffer>,
    pub surface: CFRetained<IOSurfaceRef>,
    /// Identifiant système de l'IOSurface : stable pour un tampon donné, sert de clé de cache.
    pub surface_id: u32,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
}

// Les objets CoreFoundation/CoreVideo sont comptés par référence de façon atomique et
// utilisables depuis n'importe quel thread.
unsafe impl Send for SurfaceFrame {}
unsafe impl Sync for SurfaceFrame {}

impl SurfaceFrame {
    /// Enveloppe un `CVPixelBuffer` décodé. `None` s'il n'est pas adossé à une IOSurface ou si
    /// son format n'est pas géré (on demande NV12 ou BGRA aux décodeurs).
    pub fn from_pixel_buffer(pixel_buffer: CFRetained<CVPixelBuffer>) -> Option<Self> {
        let format = match CVPixelBufferGetPixelFormatType(&pixel_buffer) {
            f if f == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
                || f == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange =>
            {
                PixelFormat::Nv12
            }
            f if f == kCVPixelFormatType_32BGRA => PixelFormat::Bgra,
            _ => return None,
        };
        let surface = CVPixelBufferGetIOSurface(Some(&pixel_buffer))?;
        let surface_id = surface.id();
        let width = CVPixelBufferGetWidth(&pixel_buffer) as u32;
        let height = CVPixelBufferGetHeight(&pixel_buffer) as u32;
        Some(Self {
            pixel_buffer,
            surface,
            surface_id,
            width,
            height,
            format,
        })
    }
}

/// Une image prête pour le rendu.
#[derive(Clone)]
pub enum Frame {
    Gst(gst::Sample),
    Surface(Arc<SurfaceFrame>),
}

pub struct FrameSlot {
    id: u64,
    seq: AtomicU64,
    latest: Mutex<Option<Frame>>,
    pub fps: FpsCounter,
}

impl Default for FrameSlot {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameSlot {
    pub fn new() -> Self {
        Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            seq: AtomicU64::new(0),
            latest: Mutex::new(None),
            fps: FpsCounter::new(),
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    fn store(&self, frame: Frame) {
        *self.latest.lock().unwrap() = Some(frame);
        self.seq.fetch_add(1, Ordering::Release);
        self.fps.tick();
    }

    /// Image GStreamer (mémoire système).
    pub fn push(&self, sample: gst::Sample) {
        self.store(Frame::Gst(sample));
    }

    /// Image décodée en mémoire GPU (zéro copie).
    pub fn push_surface(&self, frame: Arc<SurfaceFrame>) {
        self.store(Frame::Surface(frame));
    }

    pub fn clear(&self) {
        *self.latest.lock().unwrap() = None;
        self.seq.fetch_add(1, Ordering::Release);
    }

    pub fn seq(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
    }

    pub fn latest(&self) -> Option<(u64, Frame)> {
        let seq = self.seq();
        self.latest.lock().unwrap().clone().map(|f| (seq, f))
    }

    /// Dimensions de la dernière image (pour les statistiques).
    pub fn dimensions(&self) -> Option<(u32, u32)> {
        let guard = self.latest.lock().unwrap();
        match guard.as_ref()? {
            Frame::Surface(s) => Some((s.width, s.height)),
            Frame::Gst(sample) => {
                let info = gst_video::VideoInfo::from_caps(sample.caps()?).ok()?;
                Some((info.width(), info.height()))
            }
        }
    }
}

/// Format des pixels transmis au GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Rgba,
    Bgra,
    Nv12,
}

/// Plans d'une image GStreamer mappée en lecture.
pub struct Planes<'a> {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub data: Vec<(&'a [u8], u32)>,
}

/// Mappe un échantillon GStreamer et appelle `f` avec ses plans.
pub fn with_planes<R>(sample: &gst::Sample, f: impl FnOnce(Planes<'_>) -> R) -> Option<R> {
    let buffer = sample.buffer()?;
    let caps = sample.caps()?;
    let info = gst_video::VideoInfo::from_caps(caps).ok()?;
    let format = match info.format() {
        gst_video::VideoFormat::Rgba => PixelFormat::Rgba,
        gst_video::VideoFormat::Bgra => PixelFormat::Bgra,
        gst_video::VideoFormat::Nv12 => PixelFormat::Nv12,
        _ => return None,
    };
    let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info).ok()?;
    let n = frame.n_planes() as usize;
    let mut data = Vec::with_capacity(n);
    for i in 0..n {
        let d = frame.plane_data(i as u32).ok()?;
        data.push((d, frame.plane_stride()[i] as u32));
    }
    Some(f(Planes {
        width: info.width(),
        height: info.height(),
        format,
        data,
    }))
}

/// Compteur d'images par seconde (mis à jour par `tick`, lu par `value`).
pub struct FpsCounter {
    count: AtomicU64,
    state: Mutex<(Instant, f32)>,
}

impl Default for FpsCounter {
    fn default() -> Self {
        Self::new()
    }
}

impl FpsCounter {
    pub fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            state: Mutex::new((Instant::now(), 0.0)),
        }
    }

    pub fn tick(&self) {
        self.count.fetch_add(1, Ordering::Relaxed);
        let mut st = self.state.lock().unwrap();
        let elapsed = st.0.elapsed().as_secs_f32();
        if elapsed >= 1.0 {
            let n = self.count.swap(0, Ordering::Relaxed);
            *st = (Instant::now(), n as f32 / elapsed);
        }
    }

    /// Dernière valeur mesurée ; 0 si plus rien n'arrive depuis 2 s.
    pub fn value(&self) -> f32 {
        let st = self.state.lock().unwrap();
        if st.0.elapsed().as_secs_f32() > 2.0 {
            0.0
        } else {
            st.1
        }
    }
}

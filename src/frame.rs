//! Échange d'images entre les décodeurs et le rendu : chaque source (téléphone, fichier vidéo)
//! possède un emplacement contenant la dernière image reçue.
//!
//! Une image est toujours en **mémoire GPU** : un `CVPixelBuffer` décodé par VideoToolbox
//! (téléphone) ou AVFoundation (fichiers), adossé à une **IOSurface** que le rendu importe
//! directement comme texture Metal — zéro copie, le CPU ne touche jamais aux pixels.

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
/// est référencée (par l'emplacement ou par une texture importée) : sinon le décodeur
/// recyclerait le tampon sous les pieds du GPU.
pub struct SurfaceFrame {
    /// Gardé en vie pour retenir le tampon (jamais lu directement).
    _pixel_buffer: CFRetained<CVPixelBuffer>,
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
            _pixel_buffer: pixel_buffer,
            surface,
            surface_id,
            width,
            height,
            format,
        })
    }
}

pub struct FrameSlot {
    id: u64,
    seq: AtomicU64,
    latest: Mutex<Option<Arc<SurfaceFrame>>>,
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

    /// Nouvelle image décodée (mémoire GPU, zéro copie).
    pub fn push(&self, frame: Arc<SurfaceFrame>) {
        *self.latest.lock().unwrap() = Some(frame);
        self.seq.fetch_add(1, Ordering::Release);
        self.fps.tick();
    }

    pub fn clear(&self) {
        *self.latest.lock().unwrap() = None;
        self.seq.fetch_add(1, Ordering::Release);
    }

    pub fn seq(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
    }

    pub fn latest(&self) -> Option<(u64, Arc<SurfaceFrame>)> {
        let seq = self.seq();
        self.latest.lock().unwrap().clone().map(|f| (seq, f))
    }

    /// Dimensions de la dernière image (pour les statistiques).
    pub fn dimensions(&self) -> Option<(u32, u32)> {
        let guard = self.latest.lock().unwrap();
        guard.as_ref().map(|s| (s.width, s.height))
    }
}

/// Format des pixels des images décodées.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Bgra,
    Nv12,
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

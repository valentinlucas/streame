//! Échange d'images entre les threads GStreamer et le rendu : chaque source
//! (téléphone, fichier vidéo) possède un emplacement contenant la dernière image reçue.

use gst_video::prelude::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub struct FrameSlot {
    id: u64,
    seq: AtomicU64,
    latest: Mutex<Option<gst::Sample>>,
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

    pub fn push(&self, sample: gst::Sample) {
        *self.latest.lock().unwrap() = Some(sample);
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

    pub fn latest(&self) -> Option<(u64, gst::Sample)> {
        let seq = self.seq();
        self.latest.lock().unwrap().clone().map(|s| (seq, s))
    }

    /// Dimensions de la dernière image (pour les statistiques).
    pub fn dimensions(&self) -> Option<(u32, u32)> {
        let guard = self.latest.lock().unwrap();
        let sample = guard.as_ref()?;
        let info = gst_video::VideoInfo::from_caps(sample.caps()?).ok()?;
        Some((info.width(), info.height()))
    }
}

/// Format des pixels transmis au GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Rgba,
    Bgra,
    Nv12,
}

/// Plans d'une image mappée en lecture.
pub struct Planes<'a> {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub data: Vec<(&'a [u8], u32)>,
}

/// Mappe un échantillon et appelle `f` avec ses plans.
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

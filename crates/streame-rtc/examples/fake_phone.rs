//! Faux téléphone en Rust, pour tester la session cliente sur Mac sans iPhone : mire animée
//! (CVPixelBuffer NV12 générés) encodée par VideoToolbox, micro synthétique (sinus), même
//! parcours que l'app iOS (Bonjour mis à part) : `/api/config`, WebSocket `/ws`, offre du Mac,
//! réponse, ICE, RTP, statistiques.
//!
//! Usage : `cargo run -p streame-rtc --features client --example fake_phone -- [hôte] [port]`
//!   HOLD=20  durée en secondes (défaut 15) ; QUALITY=720 (défaut 1080) ; NAME=… ; FPS=30
//!
//! Prérequis : `streame --no-window` (ou la régie) qui écoute sur l'hôte/port indiqués.

use objc2_core_foundation::{CFDictionary, CFRetained, CFString, CFType};
use objc2_core_video::{
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
    CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress,
};
use std::ptr::{null_mut, NonNull};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use streame_rtc::client::{AudioBackend, Client, ClientConfig, ClientEvent};
use streame_rtc::opus::{PcmSink, PcmSource, FRAME_SAMPLES, SAMPLE_RATE};

/// Micro synthétique : sinus 440 Hz, cadencé sur l'horloge murale (comme une vraie capture).
struct Sine {
    start: Instant,
    produced: Mutex<u64>,
    phase: Mutex<f32>,
}

impl PcmSource for Sine {
    fn pull_frame(&self, out: &mut [f32]) -> bool {
        let frames = (out.len() / 2) as u64;
        let mut produced = self.produced.lock().unwrap();
        let available = (self.start.elapsed().as_secs_f64() * SAMPLE_RATE as f64) as u64;
        if available < *produced + frames {
            return false;
        }
        let mut phase = self.phase.lock().unwrap();
        for f in 0..frames as usize {
            let v = (*phase).sin() * 0.2;
            out[f * 2] = v;
            out[f * 2 + 1] = v;
            *phase += 2.0 * std::f32::consts::PI * 440.0 / SAMPLE_RATE as f32;
        }
        *phase %= 2.0 * std::f32::consts::PI;
        *produced += frames;
        true
    }
}

/// Puits du retour : compte les échantillons reçus.
struct Count(AtomicU64);
impl PcmSink for Count {
    fn push(&self, samples: &[f32]) {
        self.0.fetch_add(samples.len() as u64, Ordering::Relaxed);
    }
}

fn make_frame(width: usize, height: usize, n: u64) -> CFRetained<CVPixelBuffer> {
    let props: CFRetained<CFDictionary<CFString, CFType>> = CFDictionary::from_slices(&[], &[]);
    let attrs: CFRetained<CFDictionary<CFString, CFType>> = CFDictionary::from_slices(
        &[unsafe { kCVPixelBufferIOSurfacePropertiesKey }],
        &[&*props],
    );
    let attrs_ref: &CFDictionary = unsafe { &*CFRetained::as_ptr(&attrs).cast::<CFDictionary>().as_ptr() };
    let mut out: *mut CVPixelBuffer = null_mut();
    let status = unsafe {
        CVPixelBufferCreate(
            None,
            width,
            height,
            kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            Some(attrs_ref),
            NonNull::from(&mut out),
        )
    };
    assert_eq!(status, 0, "CVPixelBufferCreate");
    let pb = unsafe { CFRetained::from_raw(NonNull::new(out).unwrap()) };
    unsafe {
        CVPixelBufferLockBaseAddress(&pb, CVPixelBufferLockFlags(0));
        // Plan Y : dégradé + barre verticale qui défile ; plan UV : teinte qui tourne.
        let y = CVPixelBufferGetBaseAddressOfPlane(&pb, 0) as *mut u8;
        let ys = CVPixelBufferGetBytesPerRowOfPlane(&pb, 0);
        let bar = (n as usize * 8) % width;
        // Bruit pseudo-aléatoire (xorshift) renouvelé à chaque image : incompressible, pour que
        // l'encodeur consomme réellement le débit cible (une mire lisse tient en 1 Mb/s).
        let mut rng: u32 = 0x9E37_79B9 ^ (n as u32).wrapping_mul(2_654_435_761);
        for row in 0..height {
            let line = std::slice::from_raw_parts_mut(y.add(row * ys), width);
            for (x, px) in line.iter_mut().enumerate() {
                rng ^= rng << 13;
                rng ^= rng >> 17;
                rng ^= rng << 5;
                let noise = (rng & 31) as usize;
                let v = (16 + (x * 170 / width) + (row * 20 / height) + noise) as u8;
                *px = if x.abs_diff(bar) < 24 { 235 } else { v };
            }
        }
        let uv = CVPixelBufferGetBaseAddressOfPlane(&pb, 1) as *mut u8;
        let uvs = CVPixelBufferGetBytesPerRowOfPlane(&pb, 1);
        let (cb, cr) = ((128 + ((n / 4) % 60) as i32 - 30) as u8, (128 - ((n / 6) % 40) as i32 + 20) as u8);
        for row in 0..height / 2 {
            let line = std::slice::from_raw_parts_mut(uv.add(row * uvs), width);
            for p in line.chunks_mut(2) {
                p[0] = cb;
                p[1] = cr;
            }
        }
        CVPixelBufferUnlockBaseAddress(&pb, CVPixelBufferLockFlags(0));
    }
    pb
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let mut args = std::env::args().skip(1);
    let host = args.next().unwrap_or_else(|| "127.0.0.1".into());
    let port: u16 = args.next().and_then(|p| p.parse().ok()).unwrap_or(8443);
    let hold: u64 = std::env::var("HOLD").ok().and_then(|v| v.parse().ok()).unwrap_or(15);
    let quality: u32 = std::env::var("QUALITY").ok().and_then(|v| v.parse().ok()).unwrap_or(1080);
    let fps: u32 = std::env::var("FPS").ok().and_then(|v| v.parse().ok()).unwrap_or(30);
    let name = std::env::var("NAME").unwrap_or_else(|_| "Faux iPhone (Rust)".into());
    let (width, height) = match quality {
        480 => (854usize, 480usize),
        720 => (1280, 720),
        _ => (1920, 1080),
    };

    let received = Arc::new(Count(AtomicU64::new(0)));
    let cfg = ClientConfig {
        host,
        port,
        name,
        height: height as u32,
        fps,
        audio: AudioBackend::Custom {
            source: Arc::new(Sine { start: Instant::now(), produced: Mutex::new(0), phase: Mutex::new(0.0) }),
            sink: received.clone(),
        },
        mic_bitrate: 96_000,
        cert_fingerprint: None,
    };
    let connected = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let last_stats = Arc::new(Mutex::new(None));
    let client = {
        let connected = connected.clone();
        let last_stats = last_stats.clone();
        Client::start(cfg, move |ev| match ev {
            ClientEvent::Status(t) => println!("  [état] {t}"),
            ClientEvent::Connected => {
                println!("  [WebRTC] connecté");
                connected.store(true, Ordering::Relaxed);
            }
            ClientEvent::Disconnected => println!("  [WebRTC] déconnecté"),
            ClientEvent::OnAir(on) => println!("  [antenne] {}", if on { "À L'ANTENNE" } else { "hors antenne" }),
            ClientEvent::Stats(st) => {
                println!(
                    "  [stats] {}x{} · {:.0} i/s · {:.0} kb/s · {} · rtt {}",
                    st.width,
                    st.height,
                    st.fps,
                    st.bitrate_kbps,
                    st.quality_limitation,
                    st.rtt_ms.map(|r| format!("{r:.0} ms")).unwrap_or_else(|| "?".into())
                );
                *last_stats.lock().unwrap() = Some(st);
            }
            ClientEvent::Error(e) => println!("  [erreur] {e}"),
            ClientEvent::Ended(r) => println!("  [fin] {r}"),
            ClientEvent::Certificate { fingerprint, trusted } => {
                println!("  [certificat] {fingerprint} ({})", if trusted { "accepté" } else { "CHANGÉ, refusé" })
            }
        })
        .expect("démarrage du client")
    };

    // Caméra synthétique : cadence `fps`, horodatage = temps écoulé.
    let t0 = Instant::now();
    let period = Duration::from_secs_f64(1.0 / fps as f64);
    let mut n: u64 = 0;
    let mut next = t0;
    while t0.elapsed() < Duration::from_secs(hold) {
        let pb = make_frame(width, height, n);
        client.push_video(pb, t0.elapsed());
        n += 1;
        next += period;
        if let Some(d) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(d);
        }
    }
    let frames_out = client.counters().frames_out.load(Ordering::Relaxed);
    let keyframes = client.counters().keyframes.load(Ordering::Relaxed);
    let dropped = client.counters().dropped.load(Ordering::Relaxed);
    let errors = client.counters().errors.load(Ordering::Relaxed);
    println!(
        "\n=== bilan : {n} images poussées, {frames_out} encodées ({keyframes} images-clés, {dropped} sautées, {errors} erreurs), \
         retour audio reçu : {:.1} s, connecté : {}",
        received.0.load(Ordering::Relaxed) as f64 / (SAMPLE_RATE as f64 * 2.0),
        connected.load(Ordering::Relaxed)
    );
    if let Some(st) = last_stats.lock().unwrap().as_ref() {
        println!("=== dernières stats : {}x{} {:.0} i/s {:.0} kb/s ({})", st.width, st.height, st.fps, st.bitrate_kbps, st.quality_limitation);
    }
    client.stop();
    let _ = FRAME_SAMPLES;
}

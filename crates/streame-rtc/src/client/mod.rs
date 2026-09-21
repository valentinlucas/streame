//! Session « téléphone » complète : WebSocket de signaling (reconnexion automatique, certificat
//! mémorisé), une [`session::Session`] WebRTC par offre du Mac, encodeur H264 VideoToolbox sur
//! un thread dédié (avec réduction de résolution), audio par pont PCM (app) ou cpal (bancs).
//!
//! Utilisé par l'app iOS (via `crates/streame-ios`, API C) et par le banc
//! `examples/fake_phone.rs` sur Mac (mêmes API VideoToolbox/CoreAudio).
//!
//! ```text
//!  caméra (Swift) ─► push_video ─► thread « h264-encode » (VideoToolbox) ─► Session ─► RTP ─► Mac
//!  micro (cpal)   ─► anneau ─► Opus (libopus) ─────────────────────────────► Session ─► RTP ─► Mac
//!  Mac ─► RTP ─► Session ─► Opus (libopus) ─► anneau ─► sortie (cpal)
//! ```

#[cfg(feature = "cpal-audio")]
pub mod cpal_io;
pub mod keyframe;
pub mod pcm;
pub mod session;
pub mod vtenc;
pub mod ws;

use crate::opus::{PcmSink, PcmSource};
use crate::signaling::{ClientMsg, PhoneStats, ServerMsg};
use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use objc2_core_foundation::CFRetained;
use objc2_core_video::{CVPixelBuffer, CVPixelBufferGetHeight, CVPixelBufferGetWidth};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use vtenc::{scaled_dims, Counters, EncoderConfig, H264Encoder, Output, Profile, Scaler};

pub use tokio_tungstenite::tungstenite;

/// Événements vers l'interface (thread quelconque).
#[derive(Debug, Clone)]
pub enum ClientEvent {
    /// Texte d'état à afficher.
    Status(String),
    /// Connexion WebRTC établie.
    Connected,
    /// WebSocket ou WebRTC coupé, nouvelle tentative en cours.
    Disconnected,
    /// Le flux est (ou n'est plus) diffusé sur le programme du Mac.
    OnAir(bool),
    /// Statistiques d'émission (chaque seconde).
    Stats(PhoneStats),
    /// Erreur signalée par le Mac.
    Error(String),
    /// Session terminée à l'initiative du Mac (pas de reconnexion) — `reason`.
    Ended(String),
    /// Certificat présenté par la régie (empreinte SHA-256 hexadécimale). `trusted` : accepté
    /// (première connexion, ou identique à l'empreinte mémorisée) ; sinon la connexion est
    /// refusée tant que l'app n'a pas accepté la nouvelle empreinte.
    Certificate { fingerprint: String, trusted: bool },
}

pub type EventSink = Arc<dyn Fn(ClientEvent) + Send + Sync>;

/// Source/puits audio.
pub enum AudioBackend {
    /// Moteur audio de l'app (AVAudioEngine sur iOS) : il pousse le micro et tire la sortie
    /// par [`Client::audio_bridge`].
    Bridge,
    /// cpal, périphériques par défaut (bancs, Mac).
    #[cfg(feature = "cpal-audio")]
    Cpal,
    /// Source et puits fournis (banc de test).
    Custom { source: Arc<dyn PcmSource>, sink: Arc<dyn PcmSink> },
    None,
}

pub struct ClientConfig {
    /// Hôte de la régie (IP ou nom, découvert en Bonjour ou saisi).
    pub host: String,
    pub port: u16,
    /// Nom affiché dans le multiview.
    pub name: String,
    /// Hauteur d'image demandée (1080, 720, 480) : sert au plafond de débit ; la caméra est
    /// réglée par l'app.
    pub height: u32,
    pub fps: u32,
    pub audio: AudioBackend,
    /// Débit Opus du micro (bit/s).
    pub mic_bitrate: i32,
    /// Empreinte SHA-256 (hexadécimale) du certificat mémorisé pour cette régie ; `None` à la
    /// première connexion (le certificat présenté est alors remonté par
    /// [`ClientEvent::Certificate`]).
    pub cert_fingerprint: Option<String>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 8443,
            name: "iPhone".into(),
            height: 1080,
            fps: 30,
            audio: AudioBackend::Bridge,
            mic_bitrate: 96_000,
            cert_fingerprint: None,
        }
    }
}

/// Réglages de l'encodeur partagés entre les sessions, le thread d'encodage et l'interface.
pub struct EncoderControl {
    bitrate_bps: AtomicU32,
    pub max_bitrate: AtomicU64,
    force_key: AtomicBool,
    /// Hauteur maximale encodée (0 = celle de la caméra) : réduction avant l'encodeur.
    max_height: AtomicU32,
    pub profile: ProfileSlot,
}

/// `profile-level-id` négocié (chaîne courte, écrite une fois par session).
#[derive(Default)]
pub struct ProfileSlot(Mutex<String>);

impl ProfileSlot {
    pub fn store(&self, id: &str) {
        *self.0.lock().unwrap() = id.to_string();
    }
    pub fn profile(&self) -> Profile {
        Profile::from_profile_level_id(&self.0.lock().unwrap())
    }
}

impl EncoderControl {
    fn new(bitrate_bps: u32) -> Self {
        Self {
            bitrate_bps: AtomicU32::new(bitrate_bps),
            max_bitrate: AtomicU64::new(bitrate_bps as u64),
            force_key: AtomicBool::new(true),
            max_height: AtomicU32::new(0),
            profile: ProfileSlot::default(),
        }
    }
    pub fn set_max_height(&self, height: u32) {
        self.max_height.store(height, Ordering::Relaxed);
    }
    pub fn max_height(&self) -> u32 {
        self.max_height.load(Ordering::Relaxed)
    }
    pub fn set_bitrate(&self, bps: u32) {
        self.bitrate_bps.store(bps, Ordering::Relaxed);
    }
    pub fn bitrate(&self) -> u32 {
        self.bitrate_bps.load(Ordering::Relaxed)
    }
    pub fn force_keyframe(&self) {
        self.force_key.store(true, Ordering::Relaxed);
    }
}

/// Image de la caméra en attente d'encodage.
struct VideoFrame {
    pixel_buffer: CFRetained<CVPixelBuffer>,
    pts: Duration,
    captured: Instant,
}

// SAFETY: un CVPixelBuffer retenu peut être utilisé depuis un autre thread (CoreVideo est
// thread-safe pour la lecture ; VideoToolbox le lit sur ses propres threads de toute façon).
unsafe impl Send for VideoFrame {}

pub struct Client {
    rt: Option<tokio::runtime::Runtime>,
    cancel: CancellationToken,
    video_tx: std::sync::mpsc::SyncSender<VideoFrame>,
    mic_on: Arc<AtomicBool>,
    spk_on: Arc<AtomicBool>,
    encoder: Arc<EncoderControl>,
    counters: Arc<Counters>,
    bridge: Option<Arc<pcm::PcmBridge>>,
    #[cfg(feature = "cpal-audio")]
    audio: Mutex<Option<cpal_io::CpalAudio>>,
    encode_thread: Option<std::thread::JoinHandle<()>>,
    ws_task: Option<tokio::task::JoinHandle<()>>,
}

impl Client {
    /// Démarre le client : audio, thread d'encodage, boucle de signaling. Les événements sont
    /// livrés à `on_event` depuis des threads internes.
    pub fn start(cfg: ClientConfig, on_event: impl Fn(ClientEvent) + Send + Sync + 'static) -> Result<Self> {
        let events: EventSink = Arc::new(on_event);
        let cancel = CancellationToken::new();
        let mic_on = Arc::new(AtomicBool::new(true));
        let spk_on = Arc::new(AtomicBool::new(true));

        let counters = Arc::new(Counters::default());

        // --- Audio ---------------------------------------------------------------------------
        let mut bridge = None;
        #[cfg(feature = "cpal-audio")]
        let mut audio = None;
        let (mic, speaker): (Option<Arc<dyn PcmSource>>, Option<Arc<dyn PcmSink>>) = match cfg.audio {
            AudioBackend::Bridge => {
                let b = Arc::new(pcm::PcmBridge::new(mic_on.clone(), spk_on.clone(), counters.clone()));
                let (m, s) = (b.mic.clone() as Arc<dyn PcmSource>, b.speaker.clone() as Arc<dyn PcmSink>);
                bridge = Some(b);
                (Some(m), Some(s))
            }
            #[cfg(feature = "cpal-audio")]
            AudioBackend::Cpal => match cpal_io::CpalAudio::start(mic_on.clone(), spk_on.clone()) {
                Ok(a) => {
                    let (m, s) = (a.mic.clone() as Arc<dyn PcmSource>, a.speaker.clone() as Arc<dyn PcmSink>);
                    audio = Some(a);
                    (Some(m), Some(s))
                }
                Err(e) => {
                    error!("audio cpal indisponible : {e:#} (vidéo seule)");
                    events(ClientEvent::Status(format!("Audio indisponible : {e}")));
                    (None, None)
                }
            },
            AudioBackend::Custom { source, sink } => (Some(source), Some(sink)),
            AudioBackend::None => (None, None),
        };

        // --- Encodeur H264 sur son thread ------------------------------------------------------
        let encoder = Arc::new(EncoderControl::new(3_000_000));
        let output = Arc::new(Output::default());
        // Deux images en attente au plus : au-delà on jette (l'encodeur est en retard, une
        // image de plus n'aiderait pas — priorité à la latence).
        let (video_tx, video_rx) = std::sync::mpsc::sync_channel::<VideoFrame>(2);
        let encode_thread = {
            let encoder = encoder.clone();
            let output = output.clone();
            let counters = counters.clone();
            let fps = cfg.fps.max(1);
            std::thread::Builder::new()
                .name("h264-encode".into())
                .spawn(move || {
                    let mut enc: Option<H264Encoder> = None;
                    let mut scaler: Option<Scaler> = None;
                    let mut profile = Profile::Baseline;
                    let mut skipped_log = Instant::now();
                    while let Ok(frame) = video_rx.recv() {
                        if !output.is_attached() {
                            // Pas de session WebRTC : inutile d'encoder (batterie).
                            if skipped_log.elapsed() > Duration::from_secs(5) {
                                debug!("vidéo : pas de session, images ignorées");
                                skipped_log = Instant::now();
                            }
                            continue;
                        }
                        let wanted_profile = encoder.profile.profile();
                        if enc.is_none() || wanted_profile != profile {
                            profile = wanted_profile;
                            enc = Some(H264Encoder::new(
                                EncoderConfig { fps, bitrate_bps: encoder.bitrate(), profile },
                                output.clone(),
                                counters.clone(),
                            ));
                        }
                        let e = enc.as_mut().unwrap();
                        e.set_bitrate(encoder.bitrate());
                        // Réduction de résolution (plafond posé par le contrôle de débit).
                        let src_w = CVPixelBufferGetWidth(&frame.pixel_buffer) as u32;
                        let src_h = CVPixelBufferGetHeight(&frame.pixel_buffer) as u32;
                        let scaled = scaled_dims(src_w, src_h, encoder.max_height()).and_then(|(w, h)| {
                            if scaler.is_none() {
                                match Scaler::new() {
                                    Ok(s) => scaler = Some(s),
                                    Err(err) => {
                                        warn!("réduction de résolution indisponible : {err}");
                                        encoder.set_max_height(0);
                                    }
                                }
                            }
                            scaler.as_mut().and_then(|s| match s.scale(&frame.pixel_buffer, w, h) {
                                Ok(pb) => Some(pb),
                                Err(err) => {
                                    warn!("réduction {w}x{h} : {err}");
                                    None
                                }
                            })
                        });
                        let pixel_buffer: &CVPixelBuffer = scaled.as_deref().unwrap_or(&frame.pixel_buffer);
                        // Image-clé : PLI/FIR, nouvelle session, ou image jetée en aval.
                        let force = encoder.force_key.swap(false, Ordering::Relaxed) | output.take_need_keyframe();
                        if let Err(err) = e.encode(pixel_buffer, frame.pts, frame.captured, force) {
                            warn!("VideoToolbox : {err}");
                            encoder.force_keyframe();
                        }
                    }
                    info!("thread h264-encode terminé");
                })
                .context("thread h264-encode")?
        };

        // --- Runtime + boucle de signaling ------------------------------------------------------
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("streame-rtc")
            .enable_all()
            .build()
            .context("runtime tokio")?;
        let ws_task = {
            let cancel = cancel.clone();
            let expected = cfg.cert_fingerprint.as_deref().and_then(|t| {
                let fp = ws::parse_fingerprint(t);
                if fp.is_none() {
                    warn!("empreinte de certificat illisible, ignorée : {t:?}");
                }
                fp
            });
            let cert = Arc::new(ws::CertPolicy::new(expected));
            let deps = SignalingDeps {
                host: cfg.host.clone(),
                port: cfg.port,
                name: cfg.name.clone(),
                height: cfg.height,
                fps: cfg.fps.max(1),
                mic_bitrate: cfg.mic_bitrate,
                tls: ws::tls_config(cert.clone()),
                cert,
                events: events.clone(),
                encoder: encoder.clone(),
                output,
                counters: counters.clone(),
                mic,
                speaker,
            };
            rt.spawn(async move { run_signaling(deps, cancel).await })
        };

        Ok(Self {
            rt: Some(rt),
            cancel,
            video_tx,
            mic_on,
            spk_on,
            encoder,
            counters,
            bridge,
            #[cfg(feature = "cpal-audio")]
            audio: Mutex::new(audio),
            encode_thread: Some(encode_thread),
            ws_task: Some(ws_task),
        })
    }

    /// Image de la caméra (NV12 de préférence). `pts` = horodatage de présentation de la
    /// caméra. Appelable depuis la file de capture ; ne bloque jamais (image jetée si
    /// l'encodeur est en retard).
    pub fn push_video(&self, pixel_buffer: CFRetained<CVPixelBuffer>, pts: Duration) {
        let frame = VideoFrame { pixel_buffer, pts, captured: Instant::now() };
        if self.video_tx.try_send(frame).is_err() {
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Pont PCM pour le moteur audio de l'app (`AudioBackend::Bridge`).
    pub fn audio_bridge(&self) -> Option<Arc<pcm::PcmBridge>> {
        self.bridge.clone()
    }

    pub fn set_mic(&self, on: bool) {
        self.mic_on.store(on, Ordering::Relaxed);
    }

    pub fn set_speaker(&self, on: bool) {
        self.spk_on.store(on, Ordering::Relaxed);
    }

    pub fn encoder(&self) -> &EncoderControl {
        &self.encoder
    }

    pub fn counters(&self) -> &Counters {
        &self.counters
    }

    /// Arrêt : « bye » au Mac, fermeture de la session, des flux audio et du runtime.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.cancel.cancel();
        if let Some(rt) = self.rt.take() {
            if let Some(task) = self.ws_task.take() {
                // Laisse le temps d'envoyer « bye » et de fermer proprement.
                let _ = rt.block_on(async { tokio::time::timeout(Duration::from_secs(2), task).await });
            }
            rt.shutdown_timeout(Duration::from_secs(1));
        }
        // Le thread d'encodage s'arrête quand l'émetteur est fermé (drop du client) : on
        // remplace le canal pour le débloquer maintenant.
        let (tx, _rx) = std::sync::mpsc::sync_channel(1);
        let old = std::mem::replace(&mut self.video_tx, tx);
        drop(old);
        if let Some(t) = self.encode_thread.take() {
            let _ = t.join();
        }
        #[cfg(feature = "cpal-audio")]
        self.audio.lock().unwrap().take();
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Ce dont la boucle de signaling a besoin pour créer des sessions.
struct SignalingDeps {
    host: String,
    port: u16,
    name: String,
    height: u32,
    fps: u32,
    mic_bitrate: i32,
    tls: Arc<rustls::ClientConfig>,
    cert: Arc<ws::CertPolicy>,
    events: EventSink,
    encoder: Arc<EncoderControl>,
    output: Arc<Output>,
    counters: Arc<Counters>,
    mic: Option<Arc<dyn PcmSource>>,
    speaker: Option<Arc<dyn PcmSink>>,
}

/// Boucle WebSocket : connexion, `hello`, offres → sessions, reconnexion 2 s après une coupure,
/// arrêt sur `bye` du Mac ou annulation.
async fn run_signaling(deps: SignalingDeps, cancel: CancellationToken) {
    let events = deps.events.clone();
    let status = |t: String| {
        info!("{t}");
        events(ClientEvent::Status(t));
    };
    let mut first = true;
    let mut cert_announced = false;
    'outer: loop {
        if cancel.is_cancelled() {
            break;
        }
        if !first {
            events(ClientEvent::Disconnected);
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            }
        }
        first = false;
        status(format!("Connexion au Mac ({}:{})…", deps.host, deps.port));
        let server_cfg = match ws::fetch_config(&deps.host, deps.port, deps.tls.clone()).await {
            Ok(c) => c,
            Err(e) => {
                warn!("/api/config : {e:#}");
                if deps.cert.mismatch() {
                    let fingerprint = deps.cert.observed().map(|fp| ws::fingerprint_hex(&fp)).unwrap_or_default();
                    warn!("certificat de la régie changé ({fingerprint}) : connexion refusée");
                    events(ClientEvent::Certificate { fingerprint, trusted: false });
                    status("Certificat de la régie changé : connexion refusée.".into());
                } else {
                    status(format!("Mac injoignable : {e}"));
                }
                continue;
            }
        };
        if !cert_announced {
            if let Some(fp) = deps.cert.observed() {
                cert_announced = true;
                events(ClientEvent::Certificate { fingerprint: ws::fingerprint_hex(&fp), trusted: true });
            }
        }
        let socket = tokio::select! {
            _ = cancel.cancelled() => break,
            r = ws::connect(&deps.host, deps.port, deps.tls.clone()) => match r {
                Ok(s) => s,
                Err(e) => {
                    status(format!("Mac injoignable : {e}"));
                    continue;
                }
            },
        };
        let (mut sink, mut source) = socket.split();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ClientMsg>();
        let _ = out_tx.send(ClientMsg::Hello { name: Some(deps.name.clone()).filter(|n| !n.trim().is_empty()) });
        status("Connecté au Mac, en attente de l'offre…".into());
        let session_deps = session::Deps {
            ws_out: out_tx.clone(),
            events: events.clone(),
            encoder: deps.encoder.clone(),
            output: deps.output.clone(),
            counters: deps.counters.clone(),
            mic: deps.mic.clone(),
            speaker: deps.speaker.clone(),
            server_cfg,
            height: deps.height,
            fps: deps.fps,
            mic_bitrate: deps.mic_bitrate,
        };
        let mut session: Option<Arc<session::Session>> = None;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = sink.send(Message::Text(serde_json::to_string(&ClientMsg::Bye).unwrap().into())).await;
                    let _ = sink.close().await;
                    if let Some(s) = session.take() { s.close(); }
                    break 'outer;
                }
                msg = source.next() => {
                    let text = match msg {
                        Some(Ok(Message::Text(t))) => t.to_string(),
                        Some(Ok(Message::Close(_))) | None => { info!("WebSocket fermé par le Mac"); break; }
                        Some(Ok(_)) => continue,
                        Some(Err(e)) => { warn!("WebSocket : {e}"); break; }
                    };
                    let parsed: ServerMsg = match serde_json::from_str(&text) {
                        Ok(m) => m,
                        Err(e) => { warn!("message du Mac invalide : {e}"); continue; }
                    };
                    match parsed {
                        ServerMsg::Offer { sdp } => {
                            // Ré-offre (redémarrage ICE) : même session. Sinon nouvelle session.
                            if let Some(s) = session.as_ref().filter(|s| s.can_reoffer()) {
                                match s.reoffer(&sdp).await {
                                    Ok(answer) => {
                                        let _ = out_tx.send(ClientMsg::Answer { sdp: answer });
                                        status("Redémarrage ICE…".into());
                                        continue;
                                    }
                                    Err(e) => warn!("ré-offre impossible ({e:#}), nouvelle session"),
                                }
                            }
                            if let Some(old) = session.take() { old.close(); }
                            match session::Session::start(session_deps.clone(), &sdp, &cancel).await {
                                Ok((s, answer)) => {
                                    let _ = out_tx.send(ClientMsg::Answer { sdp: answer });
                                    session = Some(s);
                                    status("Négociation…".into());
                                }
                                Err(e) => {
                                    error!("session WebRTC : {e:#}");
                                    events(ClientEvent::Error(format!("{e:#}")));
                                }
                            }
                        }
                        ServerMsg::Ice { candidate, sdp_m_line_index } => {
                            if let Some(s) = &session { s.add_ice(sdp_m_line_index, &candidate).await; }
                        }
                        ServerMsg::OnAir { on } => events(ClientEvent::OnAir(on)),
                        ServerMsg::Bye { reason } => {
                            info!("bye du Mac : {reason}");
                            if let Some(s) = session.take() { s.close(); }
                            events(ClientEvent::Ended(reason));
                            let _ = sink.close().await;
                            break 'outer;
                        }
                        ServerMsg::Error { message } => {
                            warn!("erreur du Mac : {message}");
                            events(ClientEvent::Error(message));
                        }
                    }
                }
                out = out_rx.recv() => {
                    match out {
                        Some(m) => {
                            let json = serde_json::to_string(&m).unwrap_or_default();
                            if sink.send(Message::Text(json.into())).await.is_err() { break; }
                        }
                        None => break,
                    }
                }
                _ = async {
                    match &session {
                        Some(s) => s.cancel.cancelled().await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    // PeerConnection fermée (état Closed) : on repart sur une connexion complète.
                    warn!("session WebRTC fermée : reconnexion");
                    session.take();
                    let _ = sink.close().await;
                    break;
                }
            }
        }
        if let Some(s) = session.take() {
            s.close();
        }
        deps.output.detach();
    }
    deps.output.detach();
    info!("boucle de signaling terminée");
}

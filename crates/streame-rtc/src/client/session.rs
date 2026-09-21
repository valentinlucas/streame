//! Session WebRTC côté téléphone : le Mac offre, on **répond**. Audio Opus en `sendrecv`
//! (micro vers le Mac, retour du Mac), vidéo H264 en `sendonly`. Même webrtc-rs que le Mac.
//!
//! - Contrôle de congestion **GCC** côté émetteur (`Slot::CongestionControl` + pacer), nourri par
//!   les rapports TWCC que la régie envoie déjà : l'estimation pilote le débit de l'encodeur
//!   VideoToolbox entre un plancher et le plafond de la qualité choisie, et la **résolution**
//!   (échelle 1080 → 720 → 540 → 360 avec hystérésis) quand le débit ne justifie plus la pleine
//!   définition, comme libwebrtc.
//! - Les PLI/FIR du Mac remontent par [`super::keyframe::KeyframeRequests`] et forcent une IDR.
//! - Statistiques chaque seconde (`stats`) : résolution, i/s, débit, RTT, limitation.

use super::keyframe::{KeyframeRequests, SLOT as KEYFRAME_SLOT};
use super::vtenc::{Counters, EncodedFrame, Output, Profile};
use super::{ClientEvent, EncoderControl, EventSink};
use crate::h264;
use crate::opus::{self, PcmSink, PcmSource};
use crate::pc;
use crate::signaling::{ClientMsg, PhoneStats, ServerConfig};
use anyhow::{anyhow, Context, Result};
use rtc::ice::mdns::MulticastDnsMode;
use rtc::interceptor::{BandwidthEstimator, EstimatorStats, Gcc, PacketReport, Slot};
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::interceptor_registry::{
    configure_congestion_control, CongestionFeedback,
};
use rtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind,
};
use rtc::statistics::report::RTCStatsReportEntry;
use rtc::statistics::StatsSelector;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_local::{TrackLocal, TrackLocalEvent};
use webrtc::media_stream::track_remote::TrackRemote;
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionEventHandler, RTCIceCandidateInit, RTCIceConnectionState,
    RTCPeerConnectionIceErrorEvent, RTCPeerConnectionIceEvent, RTCPeerConnectionState,
    RTCSessionDescription, RTCSignalingState,
};

/// Débit plancher de l'encodeur (bit/s) : en dessous l'image n'a plus de sens.
const MIN_BITRATE: f64 = 300_000.0;

/// Hauteur maximale que justifie un débit (H264 matériel, 30 i/s) ; 0 = pas de plafond.
fn height_for_bitrate(bps: f64) -> u32 {
    if bps >= 1_500_000.0 {
        0
    } else if bps >= 700_000.0 {
        720
    } else if bps >= 400_000.0 {
        540
    } else {
        360
    }
}

/// Rang d'un plafond de hauteur (0 = illimité, le plus haut).
fn height_rank(h: u32) -> u32 {
    if h == 0 {
        u32::MAX
    } else {
        h
    }
}

/// Délai avant de descendre en résolution (débit insuffisant confirmé).
const DOWNSCALE_HOLD: Duration = Duration::from_secs(2);
/// Délai avant de remonter (débit confortable confirmé, marge de 30 %).
const UPSCALE_HOLD: Duration = Duration::from_secs(5);
const UPSCALE_MARGIN: f64 = 1.3;

/// Estimateur GCC qui publie sa cible dans un atomique lisible par la tâche de débit.
struct ReportingEstimator {
    inner: Gcc,
    target: Arc<AtomicU64>,
}

impl ReportingEstimator {
    fn publish(&self) {
        self.target
            .store(self.inner.target_bitrate().to_bits(), Ordering::Relaxed);
    }
}

impl BandwidthEstimator for ReportingEstimator {
    fn on_reports(&mut self, now: Instant, reports: &[PacketReport]) {
        self.inner.on_reports(now, reports);
        self.publish();
    }
    fn target_bitrate(&self) -> f64 {
        self.inner.target_bitrate()
    }
    fn handle_timeout(&mut self, now: Instant) {
        self.inner.handle_timeout(now);
        self.publish();
    }
    fn poll_timeout(&self) -> Option<Instant> {
        self.inner.poll_timeout()
    }
    fn stats(&self) -> EstimatorStats {
        self.inner.stats()
    }
}

/// Dépendances d'une session (fournies par le client, partagées entre les sessions).
#[derive(Clone)]
pub struct Deps {
    pub ws_out: mpsc::UnboundedSender<ClientMsg>,
    pub events: EventSink,
    pub encoder: Arc<EncoderControl>,
    pub output: Arc<Output>,
    pub counters: Arc<Counters>,
    pub mic: Option<Arc<dyn PcmSource>>,
    pub speaker: Option<Arc<dyn PcmSink>>,
    pub server_cfg: ServerConfig,
    /// Hauteur d'image demandée (plafond de débit) et cadence.
    pub height: u32,
    pub fps: u32,
    pub mic_bitrate: i32,
}

#[derive(Clone)]
struct Handler {
    deps: Deps,
    cancel: CancellationToken,
    connected: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        match event.candidate.to_json() {
            Ok(init) => {
                let _ = self.deps.ws_out.send(ClientMsg::Ice {
                    candidate: init.candidate,
                    sdp_m_line_index: init.sdp_mline_index.unwrap_or(0) as u32,
                });
            }
            Err(e) => warn!("candidat ICE → json : {e}"),
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        info!("WebRTC : état {state:?}");
        (self.deps.events)(ClientEvent::Status(format!("WebRTC : {}", state_label(state))));
        match state {
            RTCPeerConnectionState::Connected => {
                self.connected.store(true, Ordering::Relaxed);
                // Nouvelle connexion (ou ICE relancé) : image-clé pour démarrer le décodeur.
                self.deps.encoder.force_keyframe();
                (self.deps.events)(ClientEvent::Connected);
            }
            RTCPeerConnectionState::Disconnected => {
                self.connected.store(false, Ordering::Relaxed);
                warn!("WebRTC : ICE momentanément déconnecté");
            }
            RTCPeerConnectionState::Failed => {
                // Le Mac relance l'ICE (nouvelle offre) ; on attend sa ré-offre.
                self.connected.store(false, Ordering::Relaxed);
                warn!("WebRTC : ICE en échec, en attente d'un redémarrage par le Mac");
            }
            RTCPeerConnectionState::Closed => {
                self.connected.store(false, Ordering::Relaxed);
                self.cancel.cancel();
            }
            _ => {}
        }
    }

    async fn on_ice_connection_state_change(&self, state: RTCIceConnectionState) {
        info!("ICE : état {state:?}");
    }

    async fn on_ice_candidate_error(&self, event: RTCPeerConnectionIceErrorEvent) {
        warn!("erreur de candidat ICE : {event:?}");
    }

    async fn on_signaling_state_change(&self, state: RTCSignalingState) {
        debug!("signaling : état {state:?}");
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let kind = track.kind().await;
        if kind != RtpCodecKind::Audio {
            debug!("piste distante {kind:?} ignorée");
            return;
        }
        match &self.deps.speaker {
            Some(spk) => {
                info!("retour audio du Mac : piste reçue, décodage Opus");
                opus::spawn_decoder(track, spk.clone(), "retour du Mac", self.cancel.clone());
            }
            None => info!("retour audio du Mac : piste reçue, pas de sortie audio configurée"),
        }
    }
}

fn state_label(state: RTCPeerConnectionState) -> &'static str {
    match state {
        RTCPeerConnectionState::New => "nouvelle",
        RTCPeerConnectionState::Connecting => "connexion…",
        RTCPeerConnectionState::Connected => "connecté",
        RTCPeerConnectionState::Disconnected => "déconnecté",
        RTCPeerConnectionState::Failed => "échec",
        RTCPeerConnectionState::Closed => "fermé",
        _ => "?",
    }
}

pub struct Session {
    pc: Arc<dyn PeerConnection>,
    pub cancel: CancellationToken,
    connected: Arc<AtomicBool>,
    closed: AtomicBool,
    rt: tokio::runtime::Handle,
}

impl Session {
    /// Crée la session à partir de l'offre du Mac et renvoie la réponse SDP à lui envoyer.
    pub async fn start(deps: Deps, offer_sdp: &str, parent: &CancellationToken) -> Result<(Arc<Self>, String)> {
        let cancel = parent.child_token();
        let offered = h264::offered_h264(offer_sdp)
            .ok_or_else(|| anyhow!("l'offre du Mac ne contient pas de H264 packetization-mode=1"))?;
        info!(
            "offre du Mac : H264 pt={} profil {}{}",
            offered.payload_type,
            offered.profile_level_id,
            offered
                .start_bitrate_kbps
                .map(|k| format!(", débit de départ {k} kb/s"))
                .unwrap_or_default()
        );

        // --- Débits : plafond selon la qualité, départ annoncé par le Mac -------------------
        let max_kbps = h264::max_bitrate_kbps(deps.height, deps.server_cfg.video_max_bitrate_kbps).max(300);
        let start_kbps = offered
            .start_bitrate_kbps
            .or(Some(deps.server_cfg.video_start_bitrate_kbps).filter(|k| *k > 0))
            .unwrap_or(max_kbps / 2)
            .clamp(300, max_kbps);
        let max_bps = max_kbps as f64 * 1000.0;
        let start_bps = start_kbps as f64 * 1000.0;
        deps.encoder.profile.store(offered.profile_level_id.as_str());
        deps.encoder.set_bitrate(start_bps as u32);
        deps.encoder.max_bitrate.store(max_bps as u64, Ordering::Relaxed);

        // --- Media engine : codecs par défaut + le H264 exact de l'offre, GCC, PLI → app -------
        let gcc_target = Arc::new(AtomicU64::new(start_bps.to_bits()));
        let estimator = ReportingEstimator {
            inner: Gcc::new(start_bps, MIN_BITRATE, max_bps),
            target: gcc_target.clone(),
        };
        let offered_codec = h264::codec(&offered.fmtp);
        let offered_pt = offered.payload_type;
        let (media_engine, registry) = pc::media_engine_and_registry(move |registry, me| {
            me.register_codec(
                RTCRtpCodecParameters { rtp_codec: offered_codec, payload_type: offered_pt },
                RtpCodecKind::Video,
            )
            .context("enregistrement du H264 offert")?;
            let registry = configure_congestion_control(registry, estimator, CongestionFeedback::Twcc, me)
                .context("contrôle de congestion GCC")?;
            Ok(registry.with(Slot::from(KEYFRAME_SLOT), KeyframeRequests::default()))
        })?;

        let connected = Arc::new(AtomicBool::new(false));
        let handler = Arc::new(Handler {
            deps: deps.clone(),
            cancel: cancel.clone(),
            connected: connected.clone(),
        });
        // Candidats d'hôte : Wi-Fi/Ethernet seulement (`en*`). Sur iPhone, `0.0.0.0:0`
        // engloberait aussi l'adresse cellulaire, que le Mac testerait en vain.
        let mut udp_addrs = pc::bind_addrs_for_interfaces("en");
        if udp_addrs.is_empty() {
            udp_addrs.push("0.0.0.0:0".to_string());
        }
        info!("ICE : liaison sur {udp_addrs:?}");
        let pc = pc::build_peer_connection(
            pc::ice_servers(&deps.server_cfg.stun_server),
            media_engine,
            registry,
            MulticastDnsMode::Disabled,
            udp_addrs,
            handler as Arc<dyn PeerConnectionEventHandler>,
        )
        .await?;

        // --- Offre distante puis nos pistes (elles prennent les transceivers offerts) ---------
        let offer = RTCSessionDescription::offer(offer_sdp.to_string()).context("offre SDP invalide")?;
        pc.set_remote_description(offer).await.context("set-remote-description (offre)")?;

        let (audio_track, audio_ssrc) = opus::new_local_track("streame-phone", "phone-audio", "micro")?;
        let audio_sender = pc
            .add_track(audio_track.clone() as Arc<dyn TrackLocal>)
            .await
            .context("ajout de la piste audio (micro)")?;

        let video_ssrc = rand::random::<u32>();
        let video_track = Arc::new(
            TrackLocalStaticSample::new(
                Instant::now(),
                MediaStreamTrack::new(
                    "streame-phone".into(),
                    "phone-video".into(),
                    "camera".into(),
                    RtpCodecKind::Video,
                    vec![RTCRtpEncodingParameters {
                        rtp_coding_parameters: RTCRtpCodingParameters {
                            ssrc: Some(video_ssrc),
                            ..Default::default()
                        },
                        codec: h264::codec(&offered.fmtp),
                        ..Default::default()
                    }],
                ),
            )
            .context("piste vidéo locale")?,
        );
        let video_sender = pc
            .add_track(video_track.clone() as Arc<dyn TrackLocal>)
            .await
            .context("ajout de la piste vidéo")?;

        let answer = pc.create_answer(None).await.context("create-answer")?;
        pc.set_local_description(answer).await.context("set-local-description")?;
        let answer_sdp = pc
            .local_description()
            .await
            .map(|d| d.sdp)
            .ok_or_else(|| anyhow!("réponse locale absente"))?;
        debug!("réponse SDP :\n{answer_sdp}");

        // --- Micro → Opus ---------------------------------------------------------------------
        match &deps.mic {
            Some(mic) => opus::spawn_encoder(
                mic.clone(),
                audio_track,
                audio_sender,
                audio_ssrc,
                opus::EncoderOptions {
                    bitrate: deps.mic_bitrate,
                    application: ::opus::Application::Audio,
                    label: "micro",
                },
                cancel.clone(),
            ),
            None => warn!("pas de micro configuré : piste audio muette"),
        }

        // --- Caméra → VideoToolbox → RTP -------------------------------------------------------
        spawn_video_writer(video_track.clone(), video_sender.clone(), video_ssrc, deps.clone(), cancel.clone());
        spawn_keyframe_watch(video_track, deps.encoder.clone(), cancel.clone());
        spawn_bitrate_control(gcc_target, deps.encoder.clone(), cancel.clone());
        spawn_stats(pc.clone(), deps.clone(), connected.clone(), cancel.clone());

        info!("session WebRTC (webrtc-rs, répondeur) démarrée");
        Ok((
            Arc::new(Session {
                pc,
                cancel,
                connected,
                closed: AtomicBool::new(false),
                rt: tokio::runtime::Handle::current(),
            }),
            answer_sdp,
        ))
    }

    /// Le Mac relance l'ICE (nouvelle offre) : même PeerConnection, mêmes pistes.
    pub async fn reoffer(&self, offer_sdp: &str) -> Result<String> {
        let offer = RTCSessionDescription::offer(offer_sdp.to_string()).context("ré-offre invalide")?;
        self.pc.set_remote_description(offer).await.context("set-remote-description (ré-offre)")?;
        let answer = self.pc.create_answer(None).await.context("create-answer (ré-offre)")?;
        self.pc.set_local_description(answer).await.context("set-local-description (ré-offre)")?;
        self.pc
            .local_description()
            .await
            .map(|d| d.sdp)
            .ok_or_else(|| anyhow!("réponse locale absente"))
    }

    /// Une ré-offre est possible tant que la connexion n'est pas fermée.
    pub fn can_reoffer(&self) -> bool {
        !self.cancel.is_cancelled() && !self.closed.load(Ordering::Relaxed)
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    pub async fn add_ice(&self, mline: u32, candidate: &str) {
        let init = RTCIceCandidateInit {
            candidate: candidate.to_string(),
            sdp_mid: None,
            sdp_mline_index: Some(mline as u16),
            username_fragment: None,
            url: None,
        };
        if let Err(e) = self.pc.add_ice_candidate(init).await {
            warn!("add-ice-candidate : {e}");
        }
    }

    /// Ferme la session (idempotent, non bloquant).
    pub fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.cancel.cancel();
        let pc = self.pc.clone();
        self.rt.spawn(async move {
            let _ = pc.close().await;
        });
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.close();
    }
}

/// Reçoit les unités d'accès de l'encodeur et les écrit sur la piste (payloader H264 FU-A).
fn spawn_video_writer(
    track: Arc<TrackLocalStaticSample>,
    sender: Arc<dyn webrtc::rtp_transceiver::RtpSender>,
    ssrc: u32,
    deps: Deps,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        let Some(pt) = opus::resolve_payload_type(&sender, &cancel).await else {
            return;
        };
        let (tx, mut rx) = mpsc::channel::<EncodedFrame>(Output::QUEUE);
        deps.output.attach(tx);
        deps.encoder.force_keyframe();
        info!("vidéo : écriture RTP démarrée (pt={pt})");
        let mut last_pts: Option<Duration> = None;
        let nominal = Duration::from_micros(1_000_000 / deps.fps.max(1) as u64);
        let mut written: u64 = 0;
        loop {
            let frame = tokio::select! {
                _ = cancel.cancelled() => break,
                f = rx.recv() => f,
            };
            let Some(frame) = frame else { break };
            // Durée = écart de présentation avec l'image précédente (horloge caméra), bornée.
            let duration = match last_pts {
                Some(prev) if frame.pts > prev => frame.pts - prev,
                _ => nominal,
            }
            .clamp(Duration::from_millis(1), Duration::from_millis(500));
            last_pts = Some(frame.pts);
            let sample = Sample {
                data: frame.data,
                duration,
                ..Sample::new(frame.captured)
            };
            if track.sample_writer(ssrc, pt).write_sample(&sample).await.is_err() {
                break;
            }
            written += 1;
            if written == 1 {
                info!("vidéo : première image envoyée ({}x{}, image-clé : {})", frame.width, frame.height, frame.keyframe);
            }
        }
        deps.output.detach();
    });
}

/// PLI/FIR du Mac (remontés par l'intercepteur) → image-clé forcée.
fn spawn_keyframe_watch(track: Arc<TrackLocalStaticSample>, encoder: Arc<EncoderControl>, cancel: CancellationToken) {
    tokio::spawn(async move {
        loop {
            let evt = tokio::select! {
                _ = cancel.cancelled() => break,
                e = track.poll() => e,
            };
            let Some(evt) = evt else { break };
            #[allow(irrefutable_let_patterns)]
            if let TrackLocalEvent::OnRtcpPacket(packets) = evt {
                for p in packets {
                    let any = p.as_any();
                    if any.is::<PictureLossIndication>() || any.is::<FullIntraRequest>() {
                        debug!("image-clé demandée par le Mac (PLI/FIR)");
                        encoder.force_keyframe();
                    }
                }
            }
        }
    });
}

/// Applique la cible GCC à l'encodeur (4 fois par seconde), entre le plancher et le plafond,
/// et adapte le plafond de résolution avec hystérésis.
fn spawn_bitrate_control(target: Arc<AtomicU64>, encoder: Arc<EncoderControl>, cancel: CancellationToken) {
    tokio::spawn(async move {
        let mut last_logged: u32 = 0;
        let mut current_height: u32 = 0;
        let mut candidate: Option<(u32, Instant)> = None;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_millis(250)) => {}
            }
            let max = encoder.max_bitrate.load(Ordering::Relaxed) as f64;
            let est = f64::from_bits(target.load(Ordering::Relaxed));
            let bps = est.clamp(MIN_BITRATE, max.max(MIN_BITRATE)) as u32;
            encoder.set_bitrate(bps);
            if last_logged == 0 || (bps as f32 / last_logged as f32 - 1.0).abs() > 0.15 {
                debug!("débit vidéo cible : {} kb/s (estimation GCC {} kb/s)", bps / 1000, est as u32 / 1000);
                last_logged = bps;
            }
            // Résolution : on descend après 2 s sous le seuil, on remonte après 5 s avec marge.
            let down = height_for_bitrate(bps as f64);
            let up = height_for_bitrate(bps as f64 / UPSCALE_MARGIN);
            let wanted = if height_rank(down) < height_rank(current_height) {
                Some((down, DOWNSCALE_HOLD))
            } else if height_rank(up) > height_rank(current_height) {
                Some((up, UPSCALE_HOLD))
            } else {
                None
            };
            match wanted {
                Some((h, hold)) => match candidate {
                    Some((ch, since)) if ch == h => {
                        if since.elapsed() >= hold {
                            info!(
                                "résolution : plafond {} (débit {} kb/s)",
                                if h == 0 { "levé".to_string() } else { format!("{h}p") },
                                bps / 1000
                            );
                            current_height = h;
                            encoder.set_max_height(h);
                            candidate = None;
                        }
                    }
                    _ => candidate = Some((h, Instant::now())),
                },
                None => candidate = None,
            }
        }
    });
}

/// Statistiques d'émission chaque seconde → Mac (`stats`) et interface.
fn spawn_stats(pc: Arc<dyn PeerConnection>, deps: Deps, connected: Arc<AtomicBool>, cancel: CancellationToken) {
    tokio::spawn(async move {
        let mut prev_bytes: Option<(Instant, u64)> = None;
        let mut prev_frames: u64 = deps.counters.frames_out.load(Ordering::Relaxed);
        let mut prev_t = Instant::now();
        let mut prev_underruns = deps.counters.audio_underruns.load(Ordering::Relaxed);
        let mut prev_queue_dropped = deps.output.dropped();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
            if !connected.load(Ordering::Relaxed) {
                continue;
            }
            let now = Instant::now();
            let report = pc.get_stats(now, StatsSelector::None).await;
            let mut bytes_sent: Option<u64> = None;
            let mut rtt_ms: Option<f32> = None;
            let mut fraction_lost: f32 = 0.0;
            // RTT : rapports RTCP distants si disponibles, sinon la paire ICE nominée (STUN).
            for pair in report.candidate_pairs() {
                if pair.nominated && pair.current_round_trip_time > 0.0 {
                    rtt_ms = Some((pair.current_round_trip_time * 1000.0) as f32);
                }
            }
            for entry in report.iter() {
                match entry {
                    RTCStatsReportEntry::OutboundRtp(s)
                        if s.sent_rtp_stream_stats.rtp_stream_stats.kind == RtpCodecKind::Video =>
                    {
                        bytes_sent = Some(s.sent_rtp_stream_stats.bytes_sent);
                    }
                    RTCStatsReportEntry::RemoteInboundRtp(s)
                        if s.received_rtp_stream_stats.rtp_stream_stats.kind == RtpCodecKind::Video =>
                    {
                        if s.round_trip_time > 0.0 {
                            rtt_ms = Some((s.round_trip_time * 1000.0) as f32);
                        }
                        fraction_lost = s.fraction_lost as f32;
                    }
                    _ => {}
                }
            }
            let bitrate_kbps = match (prev_bytes, bytes_sent) {
                (Some((t0, b0)), Some(b1)) => {
                    let dt = now.duration_since(t0).as_secs_f32().max(0.001);
                    b1.saturating_sub(b0) as f32 * 8.0 / 1000.0 / dt
                }
                _ => 0.0,
            };
            if let Some(b) = bytes_sent {
                prev_bytes = Some((now, b));
            }
            let frames = deps.counters.frames_out.load(Ordering::Relaxed);
            let fps = (frames - prev_frames) as f32 / now.duration_since(prev_t).as_secs_f32().max(0.001);
            prev_frames = frames;
            prev_t = now;
            let target = deps.encoder.bitrate();
            let max = deps.encoder.max_bitrate.load(Ordering::Relaxed);
            // « bandwidth » si la résolution est plafonnée, ou si l'encodeur bute sur la cible
            // GCC et que celle-ci est sous le plafond ; sinon c'est le contenu qui ne demande
            // pas plus de bits.
            let at_target = bitrate_kbps * 1000.0 >= target as f32 * 0.7;
            let limitation = if fraction_lost > 0.02 {
                "bandwidth (pertes)".to_string()
            } else if deps.encoder.max_height() != 0 || (at_target && (target as u64) < max * 9 / 10) {
                "bandwidth".to_string()
            } else {
                "none".to_string()
            };
            let underruns = deps.counters.audio_underruns.load(Ordering::Relaxed);
            if underruns != prev_underruns {
                debug!("sortie audio : {} sous-alimentation(s) (re-tamponnage)", underruns.wrapping_sub(prev_underruns));
                prev_underruns = underruns;
            }
            let queue_dropped = deps.output.dropped();
            if queue_dropped != prev_queue_dropped {
                debug!("vidéo : {} image(s) jetée(s), écriture RTP en retard", queue_dropped - prev_queue_dropped);
                prev_queue_dropped = queue_dropped;
            }
            let st = PhoneStats {
                width: deps.counters.width.load(Ordering::Relaxed),
                height: deps.counters.height.load(Ordering::Relaxed),
                fps,
                bitrate_kbps,
                quality_limitation: limitation,
                rtt_ms,
                codec: "H264".into(),
            };
            let _ = deps.ws_out.send(ClientMsg::Stats(st.clone()));
            (deps.events)(ClientEvent::Stats(st));
        }
    });
}

/// Profil H264 à utiliser pour l'encodeur, dérivé de l'offre.
pub fn profile_for(profile_level_id: &str) -> Profile {
    Profile::from_profile_level_id(profile_level_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echelle_de_resolution_selon_le_debit() {
        assert_eq!(height_for_bitrate(3_000_000.0), 0);
        assert_eq!(height_for_bitrate(1_000_000.0), 720);
        assert_eq!(height_for_bitrate(500_000.0), 540);
        assert_eq!(height_for_bitrate(300_000.0), 360);
        assert!(height_rank(0) > height_rank(720) && height_rank(720) > height_rank(360));
    }
}

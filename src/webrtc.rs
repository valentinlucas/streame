//! Session WebRTC avec le téléphone, via **webrtc-rs** (crate `webrtc` + cœur sans-I/O `rtc`).
//!
//! Le Mac est l'« offreur » : il propose un flux audio bidirectionnel (retour vers le
//! téléphone) et une réception vidéo. webrtc-rs gère ICE/DTLS/SRTP/RTP et le contrôle de
//! congestion (TWCC/NACK) ; les codecs sont natifs, sans GStreamer :
//!
//! ```text
//!  Téléphone ──RTP──► webrtc-rs ──► thread h264-decode (réassemblage) ─► VideoToolbox ─► IOSurface ─► GPU
//!                              └► Opus ─► libopus ─► anneau cpal ─► carte (Wing)
//!  carte (Wing) ─► cpal ─► libopus ─► TrackLocalStaticSample ─► webrtc-rs ─► RTP ─► Téléphone
//! ```
//!
//! **Pas de jitter buffer paquet** (`Slot::JitterBuffer`) : comme dans libwebrtc, la remise en
//! ordre vidéo est faite par l'assembleur d'images (`SampleBuilder`, fenêtre de 200 ms qui couvre
//! une retransmission NACK) et la présentation par `video.av_offset_ms`. Le jitter buffer de
//! webrtc-rs donnait à tous les paquets d'une même image la même échéance et libérait donc une
//! image-clé de plusieurs centaines de paquets d'un seul bloc, ce qui débordait le canal borné
//! (256 paquets) entre le pilote et la piste : paquets jetés, image cassée, PLI, nouvelle
//! image-clé cassée… et le téléphone finissait bridé à quelques images par seconde.
//!
//! Le contrat vis-à-vis de `server.rs` (`ClientMsg`/`ServerMsg`, `start`/`set_answer`/`add_ice`/
//! `close`) est conservé ; `start`/`set_answer`/`add_ice` sont `async` (le signaling tourne déjà
//! dans une tâche Tokio).

use crate::config::Config;
use crate::engine::{Engine, PhoneStats, RtpStats};
use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use rtc::ice::mdns::MulticastDnsMode;
use rtc::media::io::sample_builder::SampleBuilder;
use rtc::peer_connection::configuration::RTCOfferOptions;
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::rtp::codec::h264::H264Packet;
use rtc::rtp_transceiver::rtp_sender::RtpCodecKind;
use rtc::rtp_transceiver::{RTCRtpTransceiverDirection, RTCRtpTransceiverInit};
use rtc::statistics::StatsSelector;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionEventHandler, RTCIceCandidateInit, RTCIceConnectionState,
    RTCPeerConnectionIceErrorEvent, RTCPeerConnectionIceEvent, RTCPeerConnectionState,
    RTCSessionDescription, RTCSignalingState,
};

// Code partagé avec l'app iOS (crates/streame-rtc) : protocole, PeerConnection, Opus, H264.
pub use streame_rtc::signaling::{ClientMsg, ServerMsg};
use streame_rtc::h264::{announce_start_bitrate, video_codec_preferences};
use streame_rtc::{opus, pc};

pub struct PhoneSession {
    pub name: String,
    pub cancel: CancellationToken,
    pc: Arc<dyn PeerConnection>,
    /// Runtime média sur lequel la session a été créée (pour fermer la PeerConnection depuis
    /// n'importe quel thread, y compris un `Drop`).
    rt: tokio::runtime::Handle,
    /// `close()` ne s'exécute qu'une fois (appelé explicitement puis par `Drop`).
    closed: AtomicBool,
}

/// Gestionnaire d'événements de la `PeerConnection`.
#[derive(Clone)]
struct Handler {
    engine: Arc<Engine>,
    out: mpsc::UnboundedSender<ServerMsg>,
    cancel: CancellationToken,
    name: String,
    /// La PeerConnection elle-même (renseignée après construction) : nécessaire pour relancer
    /// l'ICE depuis le gestionnaire d'événements.
    pc: Arc<StdMutex<Option<Arc<dyn PeerConnection>>>>,
    /// Nombre de redémarrages ICE tentés sur cette session.
    ice_restarts: Arc<AtomicU32>,
    /// Vrai tant que la connexion est établie (pour le délai de grâce du redémarrage ICE).
    connected: Arc<AtomicBool>,
    /// Codec vidéo réellement négocié (rempli à l'arrivée de la piste), pour l'affichage /control.
    video_codec: Arc<StdMutex<String>>,
    /// Retard appliqué à la vidéo pour l'aligner sur l'audio (lip-sync), en ms.
    av_offset_ms: u32,
    /// Image-clé de sécurité périodique (s), 0 = désactivée.
    keyframe_interval_s: u32,
    /// Débit vidéo de départ annoncé au téléphone (kb/s), 0 = non annoncé.
    video_start_bitrate_kbps: u32,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        match event.candidate.to_json() {
            Ok(init) => {
                let _ = self.out.send(ServerMsg::Ice {
                    candidate: init.candidate,
                    sdp_m_line_index: init.sdp_mline_index.unwrap_or(0) as u32,
                });
            }
            Err(e) => warn!("candidat ICE → json : {e}"),
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        info!("téléphone « {} » : état WebRTC {state:?}", self.name);
        match state {
            RTCPeerConnectionState::Connected => {
                self.connected.store(true, Ordering::Relaxed);
                self.engine.set_phone(Some(self.name.clone()));
            }
            RTCPeerConnectionState::Disconnected => {
                // Transitoire (perte de paquets, changement de point d'accès) : l'ICE peut se
                // rétablir seul ; on ne coupe rien, on note l'heure pour le diagnostic.
                self.connected.store(false, Ordering::Relaxed);
                warn!("téléphone « {} » : ICE momentanément déconnecté", self.name);
            }
            RTCPeerConnectionState::Failed => {
                self.connected.store(false, Ordering::Relaxed);
                self.engine.set_phone(None);
                self.ice_restart();
            }
            RTCPeerConnectionState::Closed => {
                self.connected.store(false, Ordering::Relaxed);
                self.engine.set_phone(None);
                self.cancel.cancel();
            }
            _ => {}
        }
    }

    async fn on_ice_connection_state_change(&self, state: RTCIceConnectionState) {
        info!("téléphone « {} » : état ICE {state:?}", self.name);
    }

    async fn on_ice_candidate_error(&self, event: RTCPeerConnectionIceErrorEvent) {
        warn!("téléphone « {} » : erreur de candidat ICE : {event:?}", self.name);
    }

    async fn on_signaling_state_change(&self, state: RTCSignalingState) {
        debug!("téléphone « {} » : état signaling {state:?}", self.name);
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let kind = track.kind().await;
        let media_ssrc = track.ssrcs().await.first().copied().unwrap_or(0);
        let codec = track.codec(media_ssrc).await;
        let mime = codec
            .as_ref()
            .map(|c| c.mime_type.to_lowercase())
            .unwrap_or_default();
        let clock_rate = codec
            .as_ref()
            .map(|c| c.clock_rate)
            .unwrap_or(if kind == RtpCodecKind::Audio { 48000 } else { 90000 });
        info!("piste {kind:?} « {mime} » du téléphone (ssrc {media_ssrc}, {clock_rate} Hz)");

        // Audio : décodage Opus direct (libopus) → mixeur cpal.
        if kind == RtpCodecKind::Audio {
            let engine = self.engine.clone();
            opus::spawn_decoder(
                track,
                Arc::new(move |pcm: &[f32]| engine.push_phone_audio_f32(pcm)),
                "audio téléphone",
                self.cancel.clone(),
            );
            return;
        }
        // Vidéo : seul H264 est offert (décodage matériel VideoToolbox) ; toute autre piste est
        // ignorée (le navigateur ne devrait pas pouvoir la négocier).
        if !mime.contains("h264") {
            warn!("piste vidéo « {mime} » non gérée (H264 attendu), ignorée");
            return;
        }
        *self.video_codec.lock().unwrap() = "H264".to_string();
        let keyframe_needed = Arc::new(AtomicBool::new(false));
        let decoded = Arc::new(AtomicBool::new(false));
        // Chaîne native zéro-copie : dépaquetisation en Rust → VideoToolbox → IOSurface → GPU.
        spawn_native_h264(
            track.clone(),
            self.engine.clone(),
            self.cancel.clone(),
            keyframe_needed.clone(),
            decoded.clone(),
            self.av_offset_ms,
            clock_rate,
        );
        let pli_track = Some(track);

        // Demandes d'image-clé (PLI) — une au démarrage, répétée tant que rien n'est décodé (le
        // décodeur a besoin d'une image-clé + SPS/PPS), puis trois déclencheurs, à la manière
        // d'un récepteur libwebrtc :
        //  1. **discontinuité** : le dépaquetiseur a vu une perte que NACK n'a pas rattrapée, ou
        //     VideoToolbox a échoué → PLI tout de suite ; en attendant le décodeur jette tout
        //     jusqu'à l'IDR suivante : un bref gel plutôt qu'une image pixellisée qui se propage ;
        //  2. **famine** : plus aucune image décodée (encodeur relancé, flux muet…) ;
        //  3. **filet de sécurité** périodique et lent (`keyframe_interval_s`, 0 = off) : borne
        //     toute corruption non détectée, pour un coût négligeable (1 image sur ~300 à 10 s).
        // Pas de PLI rapproché (toutes les 3 s comme avant) : chaque image-clé est lourde et
        // fait osciller la qualité.
        if let Some(pli_track) = pli_track {
            let cancel = self.cancel.clone();
            let engine = self.engine.clone();
            let interval = self.keyframe_interval_s;
            tokio::spawn(async move {
                let send_pli = |t: Arc<dyn TrackRemote>| async move {
                    let pli = PictureLossIndication {
                        sender_ssrc: 0,
                        media_ssrc,
                    };
                    t.write_rtcp(vec![Box::new(pli)]).await.is_ok()
                };
                // Démarrage : une demande, répétée toutes les 500 ms tant qu'aucune image n'a été
                // décodée (10 essais au plus). Pas de rafale inconditionnelle : chaque PLI coûte
                // une image-clé 1080p au téléphone alors que son estimation de débit part de
                // ~300 kb/s ; six d'affilée engorgeaient son pacer et retardaient la montée en
                // débit de plusieurs dizaines de secondes.
                for _ in 0..10 {
                    if decoded.load(Ordering::Relaxed) {
                        break;
                    }
                    if !send_pli(pli_track.clone()).await {
                        return;
                    }
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                    }
                }
                let mut last_pli = Instant::now();
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                    }
                    let now = Instant::now();
                    let since = now.duration_since(last_pli);
                    let reason = if keyframe_needed.swap(false, Ordering::Relaxed) && since >= Duration::from_millis(300) {
                        Some(("discontinuité (perte ou erreur de décodage)", true))
                    } else if engine.phone_slot().fps.value() <= 0.0 && since >= Duration::from_secs(2) {
                        Some(("plus aucune image décodée", true))
                    } else if interval > 0 && since >= Duration::from_secs(interval as u64) {
                        Some(("image-clé de sécurité périodique", false))
                    } else {
                        None
                    };
                    if let Some((why, loud)) = reason {
                        if loud {
                            warn!("vidéo : demande d'image-clé (PLI) — {why}");
                        } else {
                            debug!("vidéo : demande d'image-clé (PLI) — {why}");
                        }
                        if !send_pli(pli_track.clone()).await {
                            return;
                        }
                        last_pli = now;
                    }
                }
            });
        }
    }
}

impl Handler {
    /// Échec ICE : au lieu de détruire la session (et de laisser la page tout renégocier après
    /// 2 s), on relance l'ICE avec de nouveaux identifiants (nouvelle offre `ice_restart`) —
    /// les pistes, le DTLS et les décodeurs sont conservés, la coupure est bien plus courte.
    /// Deux tentatives au plus, avec un délai de grâce de 15 s chacune ; ensuite on ferme et la
    /// page se reconnecte.
    fn ice_restart(&self) {
        let n = self.ice_restarts.fetch_add(1, Ordering::Relaxed) + 1;
        let Some(pc) = self.pc.lock().unwrap().clone() else {
            self.cancel.cancel();
            return;
        };
        if n > 2 {
            warn!("téléphone « {} » : ICE en échec après {} redémarrages, session fermée", self.name, n - 1);
            self.cancel.cancel();
            return;
        }
        warn!("téléphone « {} » : ICE en échec, redémarrage ICE ({n}/2)", self.name);
        let out = self.out.clone();
        let cancel = self.cancel.clone();
        let connected = self.connected.clone();
        let engine = self.engine.clone();
        let name = self.name.clone();
        let start_kbps = self.video_start_bitrate_kbps;
        // Hors du callback (le pilote de la connexion nous appelle) : dans une tâche.
        tokio::spawn(async move {
            let opts = RTCOfferOptions {
                ice_restart: true,
                ..Default::default()
            };
            let offer = match pc.create_offer(Some(opts)).await {
                Ok(o) => o,
                Err(e) => {
                    error!("redémarrage ICE : create-offer : {e}");
                    cancel.cancel();
                    return;
                }
            };
            if let Err(e) = pc.set_local_description(offer).await {
                error!("redémarrage ICE : set-local-description : {e}");
                cancel.cancel();
                return;
            }
            match pc.local_description().await {
                Some(local) => {
                    let sdp = announce_start_bitrate(&local.sdp, start_kbps);
                    let _ = out.send(ServerMsg::Offer { sdp });
                }
                None => {
                    cancel.cancel();
                    return;
                }
            }
            tokio::select! {
                _ = cancel.cancelled() => {}
                _ = tokio::time::sleep(Duration::from_secs(15)) => {
                    if !connected.load(Ordering::Relaxed) {
                        warn!("téléphone « {name} » : pas reconnecté 15 s après le redémarrage ICE, session fermée");
                        engine.set_phone(None);
                        cancel.cancel();
                    }
                }
            }
        });
    }
}

impl PhoneSession {
    pub async fn start(
        cfg: Arc<Config>,
        name: String,
        out: mpsc::UnboundedSender<ServerMsg>,
        engine: Arc<Engine>,
    ) -> Result<Arc<Self>> {
        let cancel = CancellationToken::new();

        // --- Media engine + interceptors (NACK + TWCC + rapports) ----------------------------
        // Pas de `Slot::JitterBuffer` : voir l'en-tête du module.
        let (media_engine, registry) = pc::media_engine_and_registry(|r, _| Ok(r))?;

        // --- Gestionnaire d'événements --------------------------------------------------------
        let video_codec = Arc::new(StdMutex::new(cfg.server.video_codec.to_uppercase()));
        let pc_slot: Arc<StdMutex<Option<Arc<dyn PeerConnection>>>> = Arc::new(StdMutex::new(None));
        let handler = Arc::new(Handler {
            engine: engine.clone(),
            out: out.clone(),
            cancel: cancel.clone(),
            name: name.clone(),
            pc: pc_slot.clone(),
            ice_restarts: Arc::new(AtomicU32::new(0)),
            connected: Arc::new(AtomicBool::new(false)),
            video_codec: video_codec.clone(),
            av_offset_ms: cfg.video.av_offset_ms,
            keyframe_interval_s: cfg.server.keyframe_interval_s,
            video_start_bitrate_kbps: cfg.server.video_start_bitrate_kbps,
        });

        // --- PeerConnection -------------------------------------------------------------------
        // iOS/Safari masque ses candidats d'hôte derrière des noms mDNS `.local` : sans
        // résolution mDNS, l'ICE sur le LAN échoue. `QueryOnly` résout ceux du téléphone sans
        // annoncer les nôtres.
        let pc = pc::build_peer_connection(
            pc::ice_servers(&cfg.server.stun_server),
            media_engine,
            registry,
            MulticastDnsMode::QueryOnly,
            vec!["0.0.0.0:0".to_string()],
            handler as Arc<dyn PeerConnectionEventHandler>,
        )
        .await?;
        *pc_slot.lock().unwrap() = Some(pc.clone());

        // --- Audio retour : piste locale Opus (envoyée vers le téléphone) ---------------------
        let (return_track, ssrc) = opus::new_local_track("streame-return", "return-audio", "return")?;
        // Audio en sendrecv (envoi du retour + réception du micro du téléphone).
        let sender = pc
            .add_track(return_track.clone() as Arc<dyn TrackLocal>)
            .await
            .context("ajout de la piste audio de retour")?;
        // Vidéo en réception seule.
        let video_tr = pc
            .add_transceiver_from_kind(
                RtpCodecKind::Video,
                Some(RTCRtpTransceiverInit {
                    direction: RTCRtpTransceiverDirection::Recvonly,
                    ..Default::default()
                }),
            )
            .await
            .context("transceiver vidéo")?;
        // Codecs de l'offre : H264 seul (par défaut webrtc-rs liste VP8 en premier et un
        // navigateur qui respecte l'ordre encode alors en VP8, logiciel sur iPhone et sans
        // décodeur matériel ici). H264 = encodage matériel sur le téléphone, VideoToolbox ici.
        let prefs = video_codec_preferences(&cfg.server.h264_profile_level_id);
        if let Err(e) = video_tr.set_codec_preferences(prefs).await {
            warn!("préférence de codec vidéo refusée ({e}) : ordre par défaut de webrtc-rs");
        }

        // Son de retour → téléphone : encodage Opus (libopus) depuis l'entrée carte (cpal),
        // cadencé par la carte (même code que le micro de l'app iOS).
        match engine.return_reader() {
            Some(reader) => opus::spawn_encoder(
                Arc::new(reader),
                return_track,
                sender,
                ssrc,
                opus::EncoderOptions {
                    bitrate: cfg.server.return_audio_bitrate,
                    application: ::opus::Application::Voip,
                    label: "retour audio",
                },
                cancel.clone(),
            ),
            None => warn!("retour : entrée audio non initialisée, pas de son renvoyé"),
        }

        // --- Offre : create-offer → set-local → envoi au téléphone ---------------------------
        let offer = pc.create_offer(None).await.context("create-offer")?;
        pc.set_local_description(offer)
            .await
            .context("set-local-description")?;
        if let Some(local) = pc.local_description().await {
            let sdp = announce_start_bitrate(&local.sdp, cfg.server.video_start_bitrate_kbps);
            debug!("offre SDP envoyée :\n{sdp}");
            let _ = out.send(ServerMsg::Offer { sdp });
        } else {
            warn!("offre locale absente après set-local-description");
        }

        // --- Statistiques RTP (toutes les secondes) -------------------------------------------
        spawn_stats(pc.clone(), engine.clone(), video_codec, cancel.clone());

        info!("session WebRTC (webrtc-rs) démarrée pour « {name} »");
        Ok(Arc::new(PhoneSession {
            name,
            cancel,
            pc,
            rt: tokio::runtime::Handle::current(),
            closed: AtomicBool::new(false),
        }))
    }

    pub async fn set_answer(&self, sdp: &str) -> Result<()> {
        let answer =
            RTCSessionDescription::answer(sdp.to_string()).context("SDP de réponse invalide")?;
        self.pc
            .set_remote_description(answer)
            .await
            .context("set-remote-description")?;
        debug!("réponse SDP appliquée");
        Ok(())
    }

    pub fn set_phone_stats(&self, engine: &Engine, st: PhoneStats) {
        engine.set_phone_stats(Some(st));
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

    /// Ferme la session. Idempotent et **non bloquant** : les tâches (RTP, décodage, retour)
    /// s'arrêtent par le jeton d'annulation, la PeerConnection est fermée sur le runtime média —
    /// jamais d'attente sur un worker Tokio (un worker bloqué, c'est un serveur qui ne répond
    /// plus).
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

impl Drop for PhoneSession {
    fn drop(&mut self) {
        self.close();
    }
}

/// Vidéo H264 du téléphone en natif, sans GStreamer. Les paquets RTP sont remis en ordre et
/// réassemblés en unités d'accès par le `SampleBuilder` de webrtc-rs, décodés en matériel par
/// VideoToolbox (`vt.rs`) et livrés au rendu en IOSurface (zéro copie). Une tâche de
/// présentation applique le retard de lip-sync (`av_offset_ms`).
///
/// Le pilote webrtc-rs livre les paquets à la piste par un canal **borné** (256 paquets,
/// constante du crate) et jette ce qui ne rentre pas (« Failed to send RtpPacket to track
/// remote »). La tâche Tokio ne fait donc que vider ce canal vers une file sans limite ; le
/// réassemblage et la soumission à VideoToolbox tournent sur un thread dédié `h264-decode`,
/// hors des workers du runtime.
fn spawn_native_h264(
    track: Arc<dyn TrackRemote>,
    engine: Arc<Engine>,
    cancel: CancellationToken,
    keyframe_needed: Arc<AtomicBool>,
    decoded: Arc<AtomicBool>,
    av_offset_ms: u32,
    clock_rate: u32,
) {
    let (tx, mut rx) = mpsc::unbounded_channel::<crate::vt::Decoded>();
    {
        let engine = engine.clone();
        let cancel = cancel.clone();
        let offset = Duration::from_millis(av_offset_ms as u64);
        tokio::spawn(async move {
            loop {
                let item = tokio::select! {
                    _ = cancel.cancelled() => break,
                    r = rx.recv() => r,
                };
                let Some((arrived, frame)) = item else { break };
                if !offset.is_zero() {
                    tokio::time::sleep_until(tokio::time::Instant::from_std(arrived + offset)).await;
                }
                engine.phone_slot().push(frame);
            }
        });
    }
    let (pkt_tx, pkt_rx) = std::sync::mpsc::channel::<rtc::rtp::Packet>();
    {
        let cancel = cancel.clone();
        let spawned = std::thread::Builder::new()
            .name("h264-decode".into())
            .spawn(move || {
                let mut decoder = crate::vt::H264Decoder::new(tx);
                // `max_late` se compte en PAQUETS non consommés : il doit dépasser la taille de la
                // plus grosse image (une image-clé 1080p à haut débit fait plusieurs centaines de
                // paquets). Avec 64, toute image-clé de plus de ~75 Ko était tronquée → décodeur
                // en erreur → PLI → nouvelle image-clé tronquée… et le téléphone se bridait
                // (débit et cadence). La latence reste bornée par `with_max_time_delay`
                // (fenêtre de remise en ordre, suffisante pour une retransmission NACK).
                let mut builder = SampleBuilder::new(2048, H264Packet::default(), clock_rate.max(1))
                    .with_max_time_delay(Duration::from_millis(200));
                let mut packets: u64 = 0;
                let mut started = false;
                let mut waiting_idr = false;
                while let Ok(pkt) = pkt_rx.recv() {
                    if cancel.is_cancelled() {
                        break;
                    }
                    packets += 1;
                    if packets == 1 {
                        info!(
                            "piste video : 1er paquet RTP (pt={}), décodage VideoToolbox natif",
                            pkt.header.payload_type
                        );
                    }
                    let now = Instant::now();
                    builder.push(now, pkt);
                    while let Some(sample) = builder.pop(now) {
                        // `prev_dropped_packets` inclut les paquets de BOURRAGE (vides, envoyés par
                        // libwebrtc pour sonder le débit, surtout juste après une image-clé),
                        // comptés à part dans `prev_padding_packets` : seul le reste est une perte.
                        let lost = sample.prev_dropped_packets.saturating_sub(sample.prev_padding_packets);
                        if lost > 0 {
                            warn!(
                                "vidéo : {lost} paquet(s) perdu(s) avant l'image (non rattrapés par NACK)"
                            );
                            decoder.mark_loss();
                            keyframe_needed.store(true, Ordering::Relaxed);
                        }
                        match decoder.decode(&sample.data, sample.packet_timestamp) {
                            crate::vt::Outcome::Ok => {
                                if !started {
                                    started = true;
                                    decoded.store(true, Ordering::Relaxed);
                                    info!("vidéo : décodage VideoToolbox (matériel, IOSurface) démarré");
                                }
                                waiting_idr = false;
                            }
                            crate::vt::Outcome::NeedKeyframe => {
                                if !waiting_idr {
                                    waiting_idr = true;
                                    debug!("VideoToolbox : en attente d'une image-clé (IDR)");
                                }
                                keyframe_needed.store(true, Ordering::Relaxed);
                            }
                            crate::vt::Outcome::Error(e) => {
                                warn!("VideoToolbox : {e}");
                                keyframe_needed.store(true, Ordering::Relaxed);
                            }
                        }
                    }
                    if packets % 250 == 0 {
                        debug!("piste video : {packets} paquets RTP, {} images soumises", decoder.frames_submitted());
                    }
                }
            });
        if let Err(e) = spawned {
            error!("thread h264-decode : {e}");
            return;
        }
    }
    // Tâche Tokio : simple transfert, pour que le canal borné du pilote soit vidé au plus vite.
    tokio::spawn(async move {
        loop {
            let evt = tokio::select! {
                _ = cancel.cancelled() => break,
                evt = track.poll() => evt,
            };
            let Some(evt) = evt else {
                info!("piste video : flux terminé (poll → None)");
                break;
            };
            match evt {
                TrackRemoteEvent::OnRtpPacket(pkt) => {
                    if pkt_tx.send(pkt).is_err() {
                        break;
                    }
                }
                TrackRemoteEvent::OnEnded | TrackRemoteEvent::OnError => break,
                _ => {}
            }
        }
    });
}

/// Relève les statistiques RTP vidéo entrantes chaque seconde et les transmet au moteur.
fn spawn_stats(
    pc: Arc<dyn PeerConnection>,
    engine: Arc<Engine>,
    video_codec: Arc<StdMutex<String>>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        let mut prev: Option<(Instant, u64)> = None;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
            let report = pc.get_stats(Instant::now(), StatsSelector::None).await;
            let Some(inbound) = report
                .inbound_rtp_streams()
                .into_iter()
                .find(|s| s.received_rtp_stream_stats.rtp_stream_stats.kind == RtpCodecKind::Video)
            else {
                continue;
            };
            let recv = &inbound.received_rtp_stream_stats;
            let bytes = inbound.bytes_received;
            let now = Instant::now();
            let bitrate_kbps = match prev {
                Some((t0, b0)) => {
                    let dt = now.duration_since(t0).as_secs_f32().max(0.001);
                    bytes.saturating_sub(b0) as f32 * 8.0 / 1000.0 / dt
                }
                None => 0.0,
            };
            prev = Some((now, bytes));
            let received = recv.packets_received;
            let lost = recv.packets_lost.max(0) as u64;
            let loss_percent = if received + lost > 0 {
                lost as f32 / (received + lost) as f32 * 100.0
            } else {
                0.0
            };
            engine.set_rtp_stats(Some(RtpStats {
                codec: video_codec.lock().unwrap().clone(),
                bitrate_kbps,
                packets_received: received,
                packets_lost: recv.packets_lost,
                loss_percent,
                // `jitter` est en unités d'horodatage RTP (RFC 3550), 90 kHz pour la vidéo.
                jitter_ms: (recv.jitter / 90.0) as f32,
                nack_count: inbound.nack_count,
                pli_count: inbound.pli_count,
                rtt_ms: None,
            }));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Gestionnaire vide (les callbacks par défaut suffisent au test).
    struct NoopHandler;
    #[async_trait::async_trait]
    impl PeerConnectionEventHandler for NoopHandler {}

    /// Vérifie le chemin webrtc-rs de bout en bout côté offreur : runtime, media engine,
    /// interceptors, ajout des pistes et génération d'une offre SDP valide.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn offer_generation_smoke() {
        let (media_engine, registry) = pc::media_engine_and_registry(|r, _| Ok(r)).unwrap();
        let pc = pc::build_peer_connection(
            vec![],
            media_engine,
            registry,
            MulticastDnsMode::QueryOnly,
            vec!["0.0.0.0:0".to_string()],
            Arc::new(NoopHandler) as Arc<dyn PeerConnectionEventHandler>,
        )
        .await
        .expect("construction PeerConnection");

        let (track, _ssrc) = opus::new_local_track("s", "t", "l").unwrap();
        pc.add_track(track as Arc<dyn TrackLocal>).await.unwrap();
        pc.add_transceiver_from_kind(
            RtpCodecKind::Video,
            Some(RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Recvonly,
                ..Default::default()
            }),
        )
        .await
        .unwrap();

        let offer = pc.create_offer(None).await.expect("create-offer");
        pc.set_local_description(offer).await.expect("set-local");
        let sdp = pc.local_description().await.expect("offre locale").sdp;

        assert!(sdp.contains("m=audio"), "pas de m=audio :\n{sdp}");
        assert!(sdp.contains("m=video"), "pas de m=video :\n{sdp}");
        let low = sdp.to_lowercase();
        assert!(low.contains("opus"), "pas d'Opus dans l'offre");
        assert!(
            low.contains("h264") || low.contains("vp8"),
            "aucun codec vidéo attendu dans l'offre"
        );
        let _ = pc.close().await;
    }
}

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
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use rtc::ice::mdns::MulticastDnsMode;
use rtc::interceptor::Registry;
use rtc::media::io::sample_builder::SampleBuilder;
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::media_engine::{MIME_TYPE_H264, MIME_TYPE_OPUS};
use rtc::peer_connection::configuration::RTCOfferOptions;
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::rtp::codec::h264::H264Packet;
use rtc::rtp_transceiver::rtp_sender::{
    RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters,
    RTCRtpEncodingParameters, RtpCodecKind,
};
use rtc::rtp_transceiver::{RTCRtpTransceiverDirection, RTCRtpTransceiverInit};
use rtc::statistics::StatsSelector;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    register_default_interceptors, MediaEngine, PeerConnection, PeerConnectionBuilder,
    PeerConnectionEventHandler, RTCConfigurationBuilder, RTCIceCandidateInit, RTCIceServer,
    RTCIceConnectionState, RTCPeerConnectionIceErrorEvent, RTCPeerConnectionIceEvent,
    RTCPeerConnectionState, RTCSessionDescription, RTCSignalingState, SettingEngineBuilder,
};
use webrtc::rtp_transceiver::RtpSender;
use webrtc::runtime::{default_runtime, Runtime};

/// Messages téléphone → serveur.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello {
        #[serde(default)]
        name: Option<String>,
    },
    Answer {
        sdp: String,
    },
    Ice {
        candidate: String,
        #[serde(rename = "sdpMLineIndex")]
        sdp_m_line_index: u32,
    },
    /// Statistiques locales de la page (encodeur, réseau).
    Stats(PhoneStats),
    Bye,
}

/// Messages serveur → téléphone.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Offer {
        sdp: String,
    },
    Ice {
        candidate: String,
        #[serde(rename = "sdpMLineIndex")]
        sdp_m_line_index: u32,
    },
    Bye {
        reason: String,
    },
    Error {
        message: String,
    },
    /// Le flux du téléphone est-il diffusé sur la sortie programme du Mac.
    OnAir {
        on: bool,
    },
}

/// Runtime webrtc-rs partagé (adossé à Tokio via la feature `runtime-tokio`).
///
/// `default_runtime()` construit un handle à chaque appel ; on le résout une seule fois pour
/// tout le process, comme le recommande la doc de webrtc-rs.
fn runtime() -> Arc<dyn Runtime> {
    static RT: OnceLock<Arc<dyn Runtime>> = OnceLock::new();
    RT.get_or_init(|| default_runtime().expect("runtime webrtc-rs (feature runtime-tokio)"))
        .clone()
}

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
            spawn_opus_decode(track, self.engine.clone(), self.cancel.clone());
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
        // Chaîne native zéro-copie : dépaquetisation en Rust → VideoToolbox → IOSurface → GPU.
        spawn_native_h264(
            track.clone(),
            self.engine.clone(),
            self.cancel.clone(),
            keyframe_needed.clone(),
            self.av_offset_ms,
            clock_rate,
        );
        let pli_track = Some(track);

        // Demandes d'image-clé (PLI) — rafale au démarrage (le décodeur a besoin d'une image-clé
        // + SPS/PPS ; sans elle l'image met longtemps ou ne vient pas), puis trois déclencheurs,
        // à la manière d'un récepteur libwebrtc :
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
                // Rafale de démarrage (le décodeur a besoin d'une image-clé + SPS/PPS).
                for _ in 0..6 {
                    if !send_pli(pli_track.clone()).await {
                        return;
                    }
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        _ = tokio::time::sleep(Duration::from_millis(400)) => {}
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
                    let _ = out.send(ServerMsg::Offer { sdp: local.sdp });
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
        let runtime = runtime();
        let cancel = CancellationToken::new();

        // --- Media engine + interceptors (NACK + TWCC + rapports) ----------------------------
        // Pas de `Slot::JitterBuffer` : voir l'en-tête du module.
        let mut media_engine = MediaEngine::default();
        media_engine
            .register_default_codecs()
            .context("register_default_codecs")?;
        let registry = register_default_interceptors(Registry::new(), &mut media_engine)
            .context("interceptors par défaut")?;

        let mut ice_servers = vec![];
        let stun = cfg.server.stun_server.trim();
        if !stun.is_empty() {
            // webrtc-rs attend la forme RFC 7064 « stun:hôte:port » (sans « // »), alors que la
            // config utilise l'ancienne forme « stun://… » de webrtcbin : on normalise.
            let url = stun.replacen("://", ":", 1);
            ice_servers.push(RTCIceServer {
                urls: vec![url],
                ..Default::default()
            });
        }
        let config = RTCConfigurationBuilder::new()
            .with_ice_servers(ice_servers)
            .build();

        // iOS/Safari masque ses candidats d'hôte derrière des noms mDNS `.local` : sans
        // résolution mDNS, l'ICE sur le LAN échoue. `QueryOnly` résout ceux du téléphone sans
        // annoncer les nôtres.
        let setting_engine = SettingEngineBuilder::new()
            .with_multicast_dns_mode(MulticastDnsMode::QueryOnly)
            .with_multicast_dns_timeout(Some(Duration::from_secs(5)))
            .build();

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
        });

        // --- PeerConnection -------------------------------------------------------------------
        // `0.0.0.0:0` : une socket par interface → candidats d'hôte exploitables sur le LAN.
        let pc = PeerConnectionBuilder::new()
            .with_configuration(config)
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .with_setting_engine(setting_engine)
            .with_handler(handler as Arc<dyn PeerConnectionEventHandler>)
            .with_runtime(runtime.clone())
            .with_udp_addrs(vec!["0.0.0.0:0".to_string()])
            .build()
            .await
            .context("construction de la PeerConnection")?;
        let pc: Arc<dyn PeerConnection> = Arc::new(pc);
        *pc_slot.lock().unwrap() = Some(pc.clone());

        // --- Audio retour : piste locale Opus (envoyée vers le téléphone) ---------------------
        let ssrc = rand::random::<u32>();
        let opus_codec = RTCRtpCodec {
            mime_type: MIME_TYPE_OPUS.to_owned(),
            clock_rate: 48000,
            channels: 2,
            sdp_fmtp_line: String::new(),
            rtcp_feedback: vec![],
        };
        let return_track = Arc::new(
            TrackLocalStaticSample::new(
                Instant::now(),
                MediaStreamTrack::new(
                    "streame-return".to_string(),
                    "return-audio".to_string(),
                    "return".to_string(),
                    RtpCodecKind::Audio,
                    vec![RTCRtpEncodingParameters {
                        rtp_coding_parameters: RTCRtpCodingParameters {
                            ssrc: Some(ssrc),
                            ..Default::default()
                        },
                        codec: opus_codec,
                        ..Default::default()
                    }],
                ),
            )
            .context("piste locale de retour")?,
        );
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

        // Son de retour → téléphone : encodage Opus (libopus) depuis l'entrée carte (cpal).
        match engine.return_reader() {
            Some(reader) => spawn_opus_return(
                reader,
                return_track,
                sender,
                ssrc,
                cfg.server.return_audio_bitrate,
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
            debug!("offre SDP envoyée :\n{}", local.sdp);
            let _ = out.send(ServerMsg::Offer { sdp: local.sdp });
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

/// Codecs vidéo offerts : **H264 uniquement**, en `packetization-mode=1` (celui des iPhone),
/// le profil configuré en tête. Le décodage est matériel (VideoToolbox), qui ne gère ni VP8 ni
/// VP9 : les proposer reviendrait à risquer une négociation sans image. Les paramètres (fmtp,
/// types de charge utile) reprennent ceux enregistrés par `register_default_codecs`, sinon la
/// préférence est refusée.
fn video_codec_preferences(profile_level_id: &str) -> Vec<RTCRtpCodecParameters> {
    let fb = || {
        vec![
            RTCPFeedback { typ: "goog-remb".into(), parameter: String::new() },
            RTCPFeedback { typ: "ccm".into(), parameter: "fir".into() },
            RTCPFeedback { typ: "nack".into(), parameter: String::new() },
            RTCPFeedback { typ: "nack".into(), parameter: "pli".into() },
            RTCPFeedback { typ: "transport-cc".into(), parameter: String::new() },
        ]
    };
    let codec = |fmtp: &str, pt: u8| RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90000,
            channels: 0,
            sdp_fmtp_line: fmtp.to_owned(),
            rtcp_feedback: fb(),
        },
        payload_type: pt,
    };
    let mut out = vec![
        codec("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f", 125),
        codec("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f", 102),
        codec("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640032", 123),
    ];
    let wanted = profile_level_id.trim().to_ascii_lowercase();
    if let Some(i) = out
        .iter()
        .position(|c| c.rtp_codec.sdp_fmtp_line.ends_with(&format!("profile-level-id={wanted}")))
    {
        let first = out.remove(i);
        out.insert(0, first);
    }
    out
}

/// Attend que le type de charge utile de l'émetteur soit négocié (après la réponse SDP).
async fn resolve_payload_type(
    sender: &Arc<dyn RtpSender>,
    cancel: &CancellationToken,
) -> Option<u8> {
    loop {
        if cancel.is_cancelled() {
            return None;
        }
        if let Ok(params) = sender.get_parameters().await {
            if let Some(codec) = params.rtp_parameters.codecs.first() {
                return Some(codec.payload_type);
            }
        }
        tokio::select! {
            _ = cancel.cancelled() => return None,
            _ = tokio::time::sleep(Duration::from_millis(150)) => {}
        }
    }
}

/// Décode l'audio Opus entrant du téléphone avec libopus et le pousse dans le mixeur
/// cpal (F32 stéréo 48 kHz). Un paquet RTP Opus = une trame Opus (RFC 7587), donc pas de
/// réassemblage. Sans jitter buffer paquet, un paquet arrivé dans le désordre (rare sur un LAN)
/// est ignoré : sa trame a déjà été dissimulée par le PLC.
fn spawn_opus_decode(track: Arc<dyn TrackRemote>, engine: Arc<Engine>, cancel: CancellationToken) {
    tokio::spawn(async move {
        let mut dec = match opus::Decoder::new(48000, opus::Channels::Stereo) {
            Ok(d) => d,
            Err(e) => {
                error!("décodeur Opus : {e}");
                return;
            }
        };
        let mut pcm = vec![0f32; 5760 * 2]; // jusqu'à 120 ms stéréo @ 48 kHz
        let mut started = false;
        // Dissimulation des pertes : on suit les numéros de séquence ; sur un trou de 1 à 3
        // paquets, les trames manquantes sont reconstituées par PLC (extrapolation libopus) et
        // la dernière par le FEC en bande du paquet suivant quand le téléphone l'envoie
        // (useinbandfec=1 négocié). Sans ça, chaque paquet perdu = un trou audible.
        let mut last_seq: Option<u16> = None;
        let mut last_per_ch: usize = 960; // taille de la dernière trame (20 ms par défaut)
        let mut concealed: u64 = 0;
        loop {
            let evt = tokio::select! {
                _ = cancel.cancelled() => break,
                evt = track.poll() => evt,
            };
            let Some(evt) = evt else { break };
            match evt {
                TrackRemoteEvent::OnRtpPacket(pkt) => {
                    let seq = pkt.header.sequence_number;
                    let payload = pkt.payload.as_ref();
                    if let Some(prev) = last_seq {
                        let delta = seq.wrapping_sub(prev) as i16;
                        if delta <= 0 {
                            continue; // doublon ou paquet dans le désordre, déjà dissimulé
                        }
                        let lost = (delta - 1) as usize;
                        if (1..=3).contains(&lost) {
                            let fs = last_per_ch * 2;
                            for _ in 1..lost {
                                if let Ok(n) = dec.decode_float(&[], &mut pcm[..fs], false) {
                                    engine.push_phone_audio_f32(&pcm[..n * 2]);
                                }
                            }
                            if let Ok(n) = dec.decode_float(payload, &mut pcm[..fs], true) {
                                engine.push_phone_audio_f32(&pcm[..n * 2]);
                            }
                            concealed += lost as u64;
                            if concealed % 50 < lost as u64 {
                                debug!("audio téléphone : {concealed} trames dissimulées (PLC/FEC)");
                            }
                        }
                    }
                    last_seq = Some(seq);
                    match dec.decode_float(payload, &mut pcm, false) {
                        Ok(per_ch) => {
                            last_per_ch = per_ch.max(1);
                            engine.push_phone_audio_f32(&pcm[..per_ch * 2]);
                            if !started {
                                info!("audio téléphone : décodage Opus (libopus) démarré");
                                started = true;
                            }
                        }
                        Err(e) => warn!("décodage Opus : {e}"),
                    }
                }
                TrackRemoteEvent::OnEnded | TrackRemoteEvent::OnError => break,
                _ => {}
            }
        }
    });
}

/// Encode le retour de la carte (entrée cpal, F32 stéréo 48 kHz) en Opus avec libopus
/// et l'écrit sur la piste locale webrtc-rs, sans GStreamer. Cadence 20 ms.
fn spawn_opus_return(
    reader: crate::audio::cpal_out::Reader,
    track: Arc<TrackLocalStaticSample>,
    sender: Arc<dyn RtpSender>,
    ssrc: u32,
    bitrate: i32,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        let Some(pt) = resolve_payload_type(&sender, &cancel).await else {
            return;
        };
        let mut enc =
            match opus::Encoder::new(48000, opus::Channels::Stereo, opus::Application::Voip) {
                Ok(e) => e,
                Err(e) => {
                    error!("encodeur Opus : {e}");
                    return;
                }
            };
        let _ = enc.set_bitrate(opus::Bitrate::Bits(bitrate));
        // FEC en bande : chaque paquet emporte une version basse qualité du précédent, le
        // téléphone récupère ainsi un paquet perdu isolé sans attendre de retransmission.
        let _ = enc.set_inband_fec(true);
        let _ = enc.set_packet_loss_perc(10);
        let _ = enc.set_dtx(false);
        let frame = 960 * 2; // 20 ms stéréo @ 48 kHz
        let mut pcm = vec![0f32; frame];
        let mut out = vec![0u8; 4000];
        info!("retour audio : encodage Opus (libopus) démarré, cadencé par la carte");
        // Cadencé par la CARTE, pas par une horloge murale : on encode une trame dès que
        // 20 ms ont été capturées. Une seule horloge sur ce chemin → aucune dérive à compenser,
        // les timestamps RTP suivent exactement la capture ; le jitter buffer du téléphone
        // absorbe le réseau. Latence ≈ 1 trame + ~3 ms de scrutation.
        loop {
            if cancel.is_cancelled() {
                break;
            }
            if !reader.pull_frame(&mut pcm) {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_millis(3)) => {}
                }
                continue;
            }
            match enc.encode_float(&pcm, &mut out) {
                Ok(len) if len > 0 => {
                    let sample = Sample {
                        data: bytes::Bytes::copy_from_slice(&out[..len]),
                        duration: Duration::from_millis(20),
                        ..Sample::new(Instant::now())
                    };
                    if track
                        .sample_writer(ssrc, pt)
                        .write_sample(&sample)
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(_) => {}
                Err(e) => warn!("encodage Opus retour : {e}"),
            }
        }
    });
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
                jitter_ms: (recv.jitter * 1000.0) as f32,
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
        let runtime = runtime();

        let mut media_engine = MediaEngine::default();
        media_engine.register_default_codecs().unwrap();
        let registry = register_default_interceptors(Registry::new(), &mut media_engine).unwrap();
        let config = RTCConfigurationBuilder::new().with_ice_servers(vec![]).build();
        let setting_engine = SettingEngineBuilder::new()
            .with_multicast_dns_mode(MulticastDnsMode::QueryOnly)
            .build();

        let pc = PeerConnectionBuilder::new()
            .with_configuration(config)
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .with_setting_engine(setting_engine)
            .with_handler(Arc::new(NoopHandler) as Arc<dyn PeerConnectionEventHandler>)
            .with_runtime(runtime)
            .with_udp_addrs(vec!["0.0.0.0:0".to_string()])
            .build()
            .await
            .expect("construction PeerConnection");
        let pc: Arc<dyn PeerConnection> = Arc::new(pc);

        let opus = RTCRtpCodec {
            mime_type: MIME_TYPE_OPUS.to_owned(),
            clock_rate: 48000,
            channels: 2,
            sdp_fmtp_line: String::new(),
            rtcp_feedback: vec![],
        };
        let track = Arc::new(
            TrackLocalStaticSample::new(
                Instant::now(),
                MediaStreamTrack::new(
                    "s".into(),
                    "t".into(),
                    "l".into(),
                    RtpCodecKind::Audio,
                    vec![RTCRtpEncodingParameters {
                        rtp_coding_parameters: RTCRtpCodingParameters {
                            ssrc: Some(rand::random::<u32>()),
                            ..Default::default()
                        },
                        codec: opus,
                        ..Default::default()
                    }],
                ),
            )
            .unwrap(),
        );
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

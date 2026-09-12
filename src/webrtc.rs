//! Session WebRTC avec le téléphone, via **webrtc-rs** (crate `webrtc` + cœur sans-I/O `rtc`).
//!
//! Le Mac est l'« offreur » : il propose un flux audio bidirectionnel (retour vers le
//! téléphone) et une réception vidéo. webrtc-rs gère ICE/DTLS/SRTP/RTP, le jitter buffer et le
//! contrôle de congestion (TWCC/NACK) ; GStreamer ne fait plus que décoder et coder :
//!
//! ```text
//!  Téléphone ──RTP──► webrtc-rs (jitter buffer) ──► appsrc(x-rtp) ─► decodebin ─► vtdec ─► GPU
//!                                                                              └► opusdec ─► cpal
//!  cpal (Wing) ─► interaudiosrc ─► opusenc ─► appsink ─► TrackLocalStaticSample ─► webrtc-rs ─► RTP ─► Téléphone
//! ```
//!
//! Remplace l'ancien `webrtcbin`. Le contrat vis-à-vis de `server.rs` (`ClientMsg`/`ServerMsg`,
//! `start`/`set_answer`/`add_ice`/`close`) est conservé ; `start`/`set_answer`/`add_ice` sont
//! désormais `async` (le signaling tourne déjà dans une tâche Tokio).

use crate::config::Config;
use crate::engine::{frame_appsink, Engine, PhoneStats, RtpStats};
use anyhow::{Context, Result};
use gst::prelude::*;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use rtc::ice::mdns::MulticastDnsMode;
use rtc::interceptor::{JitterBufferBuilder, Registry, Slot};
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::RTCOfferOptions;
use rtc::peer_connection::configuration::media_engine::{
    MIME_TYPE_H264, MIME_TYPE_OPUS, MIME_TYPE_VP8, MIME_TYPE_VP9,
};
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::rtp_transceiver::rtp_sender::{
    RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters,
    RTCRtpEncodingParameters, RtpCodecKind,
};
use rtc::rtp_transceiver::{RTCRtpTransceiverDirection, RTCRtpTransceiverInit};
use rtc::shared::marshal::{Marshal, MarshalSize};
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
    /// Un pipeline de décodage **par piste entrante** (audio, vidéo). Chaque piste est isolée :
    /// deux pistes partageant un seul pipeline se gênaient (l'autoplug de la 2e decodebin
    /// échouait par intermittence → image OU son manquant selon la piste arrivée en second).
    decode_pipelines: Arc<StdMutex<Vec<gst::Pipeline>>>,
    /// Pipeline GStreamer du son de retour (hors macOS uniquement ; sur macOS le retour est
    /// encodé en Opus par libopus dans une tâche, sans pipeline).
    return_pipeline: Option<gst::Pipeline>,
}

/// Gestionnaire d'événements de la `PeerConnection`.
#[derive(Clone)]
struct Handler {
    engine: Arc<Engine>,
    out: mpsc::UnboundedSender<ServerMsg>,
    decode_pipelines: Arc<StdMutex<Vec<gst::Pipeline>>>,
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
        let (media, encoding) = rtp_media_encoding(kind, &mime);
        if media.is_empty() {
            warn!("piste entrante de type inconnu ({mime}), ignorée");
            return;
        }
        if media == "video" {
            *self.video_codec.lock().unwrap() = encoding.to_string();
        }
        info!("piste {media}/{encoding} du téléphone (ssrc {media_ssrc}, {clock_rate} Hz)");

        // Audio sur macOS : décodage Opus direct (libopus) → mixeur cpal, sans GStreamer.
        // (La vidéo reste décodée par GStreamer/vtdec ; hors macOS, l'audio aussi.)
        #[cfg(target_os = "macos")]
        if media == "audio" {
            spawn_opus_decode(track, self.engine.clone(), self.cancel.clone());
            return;
        }

        // Un pipeline dédié par piste (voir PhoneSession::decode_pipelines) : isolation totale,
        // pas d'interférence entre l'audio et la vidéo au démarrage. Les caps RTP complètes
        // (fmtp : packetization-mode, profile-level-id, sprop…) sont posées dès la création —
        // sans elles rtph264depay peut mal réassembler les paquets FU-A de l'iPhone.
        let fmtp = codec
            .as_ref()
            .map(|c| c.sdp_fmtp_line.clone())
            .unwrap_or_default();
        let keyframe_needed = Arc::new(AtomicBool::new(false));
        let params = DecodeParams {
            media,
            encoding,
            clock_rate,
            fmtp: &fmtp,
            av_offset_ms: self.av_offset_ms,
        };
        let pipe_name = format!("decode-{}-{media}", self.name.replace(' ', "_"));
        let appsrc = match build_decode_chain(&pipe_name, &params, &self.engine, keyframe_needed.clone()) {
            Ok((pipe, appsrc)) => {
                self.decode_pipelines.lock().unwrap().push(pipe);
                appsrc
            }
            Err(e) => {
                error!("mise en place du décodage {media}/{encoding} : {e:#}");
                return;
            }
        };

        // Boucle de lecture RTP : les paquets sortent déjà ordonnés/lissés du jitter buffer.
        let media_owned = media.to_string();
        let cancel = self.cancel.clone();
        // Piste conservée pour l'envoi périodique de PLI (vidéo) ; la boucle ci-dessous consomme `track`.
        let pli_track = if media == "video" {
            Some(track.clone())
        } else {
            None
        };
        tokio::spawn(async move {
            let mut count: u64 = 0u64;
            loop {
                let evt = tokio::select! {
                    _ = cancel.cancelled() => break,
                    evt = track.poll() => evt,
                };
                let Some(evt) = evt else {
                    info!("piste {media_owned} : flux terminé (poll → None)");
                    break;
                };
                match evt {
                    TrackRemoteEvent::OnRtpPacket(pkt) => {
                        if count == 0 {
                            info!("piste {media_owned} : 1er paquet RTP (pt={})", pkt.header.payload_type);
                        }
                        let mut bytes = vec![0u8; pkt.marshal_size()];
                        match pkt.marshal_to(&mut bytes) {
                            Ok(n) => {
                                bytes.truncate(n);
                                let buf = gst::Buffer::from_mut_slice(bytes);
                                match appsrc.push_buffer(buf) {
                                    Ok(_) => {
                                        count += 1;
                                        if count % 250 == 0 {
                                            debug!("piste {media_owned} : {count} paquets RTP poussés");
                                        }
                                    }
                                    Err(e) => {
                                        warn!("piste {media_owned} : push appsrc échoué ({e:?})");
                                        break;
                                    }
                                }
                            }
                            Err(e) => warn!("RTP marshal : {e}"),
                        }
                    }
                    TrackRemoteEvent::OnEnded | TrackRemoteEvent::OnError => {
                        info!("piste {media_owned} : événement {evt:?}");
                        break;
                    }
                    _ => {}
                }
            }
            let _ = appsrc.end_of_stream();
        });

        // Vidéo : demander une image-clé. On envoie un PLI tout de suite (le décodeur a besoin
        // d'une image-clé + SPS/PPS pour démarrer), puis une petite rafale, puis un rythme lent.
        // Sans cette rafale initiale, si la 1re image-clé arrive avant que decodebin ne soit prêt,
        // rtph264depay attend la suivante et l'image met longtemps (ou ne vient pas) — d'où les
        // démarrages « sans image » qu'on ne récupérait qu'en relançant le stream.
        // Demandes d'image-clé (PLI) — trois déclencheurs, à la manière d'un récepteur libwebrtc :
        //  1. **discontinuité** : le dépayloadeur (request-keyframe) a vu une perte que NACK n'a
        //     pas rattrapée et a émis un ForceKeyUnit vers l'amont → on relaie en PLI, tout de
        //     suite ; en attendant il jette les images (wait-for-keyframe) : un bref gel plutôt
        //     qu'une image pixellisée qui se propage ;
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
                        Some(("discontinuité détectée par le dépayloadeur", true))
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

        // --- Media engine + interceptors (NACK + TWCC + rapports) + jitter buffer -------------
        let mut media_engine = MediaEngine::default();
        media_engine
            .register_default_codecs()
            .context("register_default_codecs")?;
        let registry = register_default_interceptors(Registry::new(), &mut media_engine)
            .context("interceptors par défaut")?;
        let depth = Duration::from_millis(cfg.server.rtc_latency_ms.max(1) as u64);
        let registry = registry.with(
            Slot::JitterBuffer,
            JitterBufferBuilder::new().with_depth(depth).build(),
        );

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
        let decode_pipelines: Arc<StdMutex<Vec<gst::Pipeline>>> = Arc::new(StdMutex::new(Vec::new()));
        let video_codec = Arc::new(StdMutex::new(cfg.server.video_codec.to_uppercase()));
        let pc_slot: Arc<StdMutex<Option<Arc<dyn PeerConnection>>>> = Arc::new(StdMutex::new(None));
        let handler = Arc::new(Handler {
            engine: engine.clone(),
            out: out.clone(),
            decode_pipelines: decode_pipelines.clone(),
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
        // Ordre de préférence des codecs dans l'offre : par défaut webrtc-rs liste VP8 en premier
        // et un navigateur qui respecte l'ordre (Brave/Chrome, Safari) encode alors en VP8 —
        // logiciel sur iPhone, et pas de vtdec côté Mac. On met le codec configuré (H264 : encodage
        // matériel sur le téléphone, décodage matériel VideoToolbox ici) en tête.
        let prefs = video_codec_preferences(&cfg.server.video_codec);
        if let Err(e) = video_tr.set_codec_preferences(prefs).await {
            warn!("préférence de codec vidéo refusée ({e}) : ordre par défaut de webrtc-rs");
        }

        // Son de retour → téléphone. macOS : encodage Opus direct (libopus) depuis l'entrée
        // carte (cpal), sans GStreamer. Ailleurs : pipeline GStreamer (interaudiosrc → opusenc).
        #[cfg(target_os = "macos")]
        let return_pipeline: Option<gst::Pipeline> = {
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
            None
        };
        #[cfg(not(target_os = "macos"))]
        let return_pipeline: Option<gst::Pipeline> = {
            let (pipe, ret_rx) = build_return_pipeline(&cfg).context("pipeline de retour")?;
            spawn_return_writer(return_track, sender, ssrc, ret_rx, cancel.clone());
            spawn_bus_watch(&pipe, format!("return-{name}"));
            pipe.set_state(gst::State::Playing)
                .context("démarrage du pipeline de retour")?;
            Some(pipe)
        };

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
            decode_pipelines,
            return_pipeline,
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

    /// Ferme la session. Idempotent et **non bloquant** : les arrêts GStreamer (`set_state(Null)`
    /// attend la fin des threads de flux, parfois des centaines de ms avec VideoToolbox) sont
    /// faits sur un thread dédié, jamais sur un worker Tokio — un worker bloqué, c'est un
    /// serveur qui ne répond plus. La PeerConnection est fermée sur le runtime média.
    pub fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.cancel.cancel();
        let mut pipes: Vec<gst::Pipeline> = self.decode_pipelines.lock().unwrap().clone();
        if let Some(rp) = &self.return_pipeline {
            pipes.push(rp.clone());
        }
        let name = self.name.clone();
        let _ = std::thread::Builder::new()
            .name("gst-teardown".into())
            .spawn(move || {
                for pipe in &pipes {
                    if let Some(b) = pipe.bus() {
                        b.set_flushing(true); // débloque le thread de surveillance (iter_timed)
                    }
                    let _ = pipe.set_state(gst::State::Null);
                }
                debug!("session « {name} » : {} pipeline(s) GStreamer arrêté(s)", pipes.len());
            });
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

/// Journalise les erreurs/avertissements d'un pipeline GStreamer sur un thread dédié.
fn spawn_bus_watch(pipeline: &gst::Pipeline, name: String) {
    let Some(bus) = pipeline.bus() else { return };
    let _ = std::thread::Builder::new()
        .name(format!("bus-{name}"))
        .spawn(move || {
            for msg in bus.iter_timed(gst::ClockTime::NONE) {
                use gst::MessageView;
                match msg.view() {
                    MessageView::Error(e) => {
                        let src = e.src().map(|s| s.path_string().to_string()).unwrap_or_default();
                        error!("pipeline {name} : {} [{src}] ({:?})", e.error(), e.debug());
                    }
                    MessageView::Warning(w) => {
                        warn!("pipeline {name} : {} ({:?})", w.error(), w.debug());
                    }
                    MessageView::Eos(_) => {
                        info!("pipeline {name} : fin de flux");
                        break;
                    }
                    _ => {}
                }
            }
        });
}

/// Liste ordonnée des codecs vidéo à offrir, le codec configuré en tête. Les paramètres
/// (fmtp, types de charge utile) reprennent ceux enregistrés par `register_default_codecs`,
/// sinon la préférence est refusée. Variantes H264 en `packetization-mode=1` (celle des iPhone)
/// d'abord ; VP8/VP9 gardés en repli pour un navigateur sans H264 (Chromium de test, par ex.).
fn video_codec_preferences(preferred: &str) -> Vec<RTCRtpCodecParameters> {
    let fb = || {
        vec![
            RTCPFeedback { typ: "goog-remb".into(), parameter: String::new() },
            RTCPFeedback { typ: "ccm".into(), parameter: "fir".into() },
            RTCPFeedback { typ: "nack".into(), parameter: String::new() },
            RTCPFeedback { typ: "nack".into(), parameter: "pli".into() },
            RTCPFeedback { typ: "transport-cc".into(), parameter: String::new() },
        ]
    };
    let codec = |mime: &str, fmtp: &str, pt: u8| RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: mime.to_owned(),
            clock_rate: 90000,
            channels: 0,
            sdp_fmtp_line: fmtp.to_owned(),
            rtcp_feedback: fb(),
        },
        payload_type: pt,
    };
    let h264 = [
        codec(MIME_TYPE_H264, "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f", 125),
        codec(MIME_TYPE_H264, "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f", 102),
        codec(MIME_TYPE_H264, "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640032", 123),
    ];
    let vp8 = [codec(MIME_TYPE_VP8, "", 96)];
    let vp9 = [codec(MIME_TYPE_VP9, "profile-id=0", 98)];
    let mut out = Vec::new();
    match preferred.to_ascii_uppercase().as_str() {
        "VP8" => {
            out.extend(vp8);
            out.extend(h264);
            out.extend(vp9);
        }
        "VP9" => {
            out.extend(vp9);
            out.extend(h264);
            out.extend(vp8);
        }
        _ => {
            out.extend(h264);
            out.extend(vp8);
            out.extend(vp9);
        }
    }
    out
}

/// `(media, encoding-name)` GStreamer pour un type MIME webrtc-rs (ex. « video/h264 »).
fn rtp_media_encoding(kind: RtpCodecKind, mime: &str) -> (&'static str, &'static str) {
    let m = mime;
    if kind == RtpCodecKind::Audio || m.contains("opus") {
        return ("audio", "OPUS");
    }
    if m.contains("h264") {
        ("video", "H264")
    } else if m.contains("h265") || m.contains("hevc") {
        ("video", "H265")
    } else if m.contains("vp9") {
        ("video", "VP9")
    } else if m.contains("vp8") {
        ("video", "VP8")
    } else if m.contains("av1") {
        ("video", "AV1")
    } else if kind == RtpCodecKind::Video {
        ("video", "H264")
    } else {
        ("", "")
    }
}

/// Paramètres d'une piste entrante pour construire sa chaîne de décodage.
struct DecodeParams<'a> {
    media: &'a str,
    encoding: &'a str,
    clock_rate: u32,
    /// Ligne fmtp négociée (`packetization-mode=1;profile-level-id=…`), reportée dans les caps.
    fmtp: &'a str,
    /// Retard d'affichage de la vidéo pour l'aligner sur l'audio (0 = au plus tôt).
    av_offset_ms: u32,
}

/// Renvoie le premier élément GStreamer disponible parmi des candidats (décodeur matériel
/// d'abord, logiciel en repli).
fn make_first(names: &[&str]) -> Result<gst::Element> {
    for n in names {
        if let Ok(e) = gst::ElementFactory::make(n).build() {
            return Ok(e);
        }
    }
    Err(anyhow::anyhow!("aucun élément GStreamer disponible parmi {names:?}"))
}

/// Dépayloadeur RTP configuré pour la résilience : sur une discontinuité (perte non rattrapée
/// par NACK) il émet un ForceKeyUnit vers l'amont — relayé en PLI par la boucle de la piste —
/// et jette les images jusqu'à la prochaine image-clé (bref gel plutôt qu'image corrompue).
/// Propriétés disponibles à partir de GStreamer 1.20 ; ignorées sinon.
fn make_depay(name: &str) -> Result<gst::Element> {
    let el = gst::ElementFactory::make(name).build()?;
    for prop in ["request-keyframe", "wait-for-keyframe"] {
        if el.has_property(prop) {
            el.set_property(prop, true);
        }
    }
    Ok(el)
}

/// Construit un **pipeline dédié** pour une piste entrante et le démarre.
///
/// Vidéo : chaîne **explicite** `appsrc(x-rtp) → dépay → parseur → décodeur (VideoToolbox) →
/// queue → videoconvert → GPU`, câblée en entier avant le passage en PLAYING — déterministe,
/// sans l'autoplug ni la `multiqueue` de decodebin (quelques dizaines de ms de moins). Les caps
/// RTP complètes sont posées dès la création. Si `av_offset_ms` > 0, le sink est synchronisé
/// sur l'horloge avec cette latence : chaque image est affichée `av_offset_ms` après son
/// arrivée, ce qui l'aligne sur le son (dont la lecture est tamponnée d'autant).
/// Codec non prévu : repli sur decodebin (autoplug).
fn build_decode_chain(
    name: &str,
    p: &DecodeParams<'_>,
    engine: &Arc<Engine>,
    keyframe_needed: Arc<AtomicBool>,
) -> Result<(gst::Pipeline, gst_app::AppSrc)> {
    let pipe = gst::Pipeline::with_name(name);

    let mut cb = gst::Caps::builder("application/x-rtp")
        .field("media", p.media)
        .field("encoding-name", p.encoding)
        .field("clock-rate", p.clock_rate as i32);
    for kv in p.fmtp.split(';') {
        if let Some((k, v)) = kv.split_once('=') {
            let (k, v) = (k.trim(), v.trim());
            if !k.is_empty() && !v.is_empty() {
                cb = cb.field(k, v);
            }
        }
    }
    let caps = cb.build();
    let is_video = p.media == "video";
    let offset_ns = if is_video {
        p.av_offset_ms as i64 * 1_000_000
    } else {
        0
    };
    let appsrc = gst_app::AppSrc::builder()
        .caps(&caps)
        .is_live(true)
        .format(gst::Format::Time)
        .do_timestamp(true)
        .min_latency(offset_ns)
        .max_latency(offset_ns)
        .build();
    let src: gst::Element = appsrc.clone().upcast();

    // Relais des demandes d'image-clé du dépayloadeur (événement amont ForceKeyUnit).
    if let Some(pad) = src.static_pad("src") {
        let flag = keyframe_needed;
        pad.add_probe(gst::PadProbeType::EVENT_UPSTREAM, move |_, info| {
            if let Some(gst::PadProbeData::Event(ev)) = &info.data {
                if gst_video::UpstreamForceKeyUnitEvent::parse(ev).is_ok() {
                    flag.store(true, Ordering::Relaxed);
                }
            }
            gst::PadProbeReturn::Ok
        });
    }

    let mut chain: Vec<gst::Element> = vec![src.clone()];
    let explicit = match (is_video, p.encoding) {
        (true, "H264") => {
            chain.push(make_depay("rtph264depay")?);
            chain.push(gst::ElementFactory::make("h264parse").build()?);
            chain.push(make_first(&["vtdec_hw", "vtdec", "avdec_h264"])?);
            true
        }
        (true, "H265") => {
            chain.push(make_depay("rtph265depay")?);
            chain.push(gst::ElementFactory::make("h265parse").build()?);
            chain.push(make_first(&["vtdec_hw", "vtdec", "avdec_h265"])?);
            true
        }
        (true, "VP8") => {
            chain.push(make_depay("rtpvp8depay")?);
            chain.push(make_first(&["vp8dec", "avdec_vp8"])?);
            true
        }
        (true, "VP9") => {
            chain.push(make_depay("rtpvp9depay")?);
            chain.push(make_first(&["vp9dec", "vtdec_hw", "vtdec", "avdec_vp9"])?);
            true
        }
        _ => false,
    };

    if !explicit {
        // Repli générique (audio hors macOS, codec inconnu) : decodebin auto-branche.
        let decode = gst::ElementFactory::make("decodebin").build()?;
        pipe.add_many([&src, &decode])?;
        src.link(&decode)?;
        let engine2 = engine.clone();
        let pipe2 = pipe.clone();
        decode.connect_pad_added(move |_, dpad| {
            if let Err(e) = attach_decoded_branch(&pipe2, dpad, &engine2) {
                error!("branche de décodage : {e:#}");
            }
        });
    } else {
        // Découple le décodeur de l'attente de synchronisation du sink ; jette les images en
        // retard plutôt que d'accumuler.
        let q = gst::ElementFactory::make("queue")
            .property("max-size-buffers", 3u32)
            .property("max-size-time", 0u64)
            .property("max-size-bytes", 0u32)
            .property_from_str("leaky", "downstream")
            .build()?;
        chain.push(q);
        chain.push(gst::ElementFactory::make("videoconvert").build()?);
        chain.push(
            gst::ElementFactory::make("capsfilter")
                .property("caps", crate::engine::gpu_caps())
                .build()?,
        );
        chain.push(frame_appsink(engine.phone_slot().clone(), offset_ns > 0).upcast());
        let refs: Vec<&gst::Element> = chain.iter().collect();
        pipe.add_many(&refs)?;
        gst::Element::link_many(&refs)?;
        info!(
            "chaîne vidéo {} câblée ({} éléments, décodeur {}, retard A/V {} ms)",
            p.encoding,
            chain.len(),
            chain[3].factory().map(|f| f.name().to_string()).unwrap_or_default(),
            p.av_offset_ms
        );
    }

    spawn_bus_watch(&pipe, name.to_string());
    pipe.set_state(gst::State::Playing)
        .with_context(|| format!("démarrage du pipeline de décodage « {name} »"))?;
    Ok((pipe, appsrc))
}

/// Construit la branche de décodage à partir d'un pad `decodebin` (repli codec inconnu).
fn attach_decoded_branch(pipe: &gst::Pipeline, dpad: &gst::Pad, engine: &Arc<Engine>) -> Result<()> {
    let Some(caps) = dpad.current_caps() else {
        return Ok(());
    };
    let Some(s) = caps.structure(0) else {
        return Ok(());
    };
    let media = s.name().to_string();
    info!("decodebin : pad ajouté, caps décodées = {caps}");
    let q = gst::ElementFactory::make("queue")
        .property("max-size-buffers", 2u32)
        .property("max-size-time", 0u64)
        .property("max-size-bytes", 0u32)
        .property_from_str("leaky", "downstream")
        .build()?;
    let chain: Vec<gst::Element> = if media.starts_with("video/") {
        // Décodé (vtdec → NV12) puis envoyé tel quel au GPU, sans synchronisation : le jitter
        // buffer webrtc-rs a déjà lissé le flux, on affiche au plus tôt.
        let conv = gst::ElementFactory::make("videoconvert").build()?;
        let cf = gst::ElementFactory::make("capsfilter")
            .property("caps", crate::engine::gpu_caps())
            .build()?;
        let sink = frame_appsink(engine.phone_slot().clone(), false);
        vec![q, conv, cf, sink.upcast()]
    } else if media.starts_with("audio/") {
        // Le son décodé (F32 stéréo 48 kHz) est poussé dans le moteur (mixage cpal).
        let conv = gst::ElementFactory::make("audioconvert").build()?;
        let res = gst::ElementFactory::make("audioresample").build()?;
        let caps = gst::Caps::builder("audio/x-raw")
            .field("format", "F32LE")
            .field("layout", "interleaved")
            .field("rate", 48000i32)
            .field("channels", 2i32)
            .build();
        let cf = gst::ElementFactory::make("capsfilter")
            .property("caps", &caps)
            .build()?;
        let sink = gst_app::AppSink::builder()
            .caps(&caps)
            .sync(false)
            .max_buffers(4)
            .drop(true)
            .build();
        let eng = engine.clone();
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |s| {
                    let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    if let Some(buf) = sample.buffer_owned() {
                        eng.push_phone_audio(buf);
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        vec![q, conv, res, cf, sink.upcast()]
    } else {
        return Ok(());
    };
    let refs: Vec<&gst::Element> = chain.iter().collect();
    pipe.add_many(&refs)?;
    gst::Element::link_many(&refs)?;
    for el in &chain {
        el.sync_state_with_parent()?;
    }
    dpad.link(&chain[0].static_pad("sink").unwrap())?;
    info!("flux {media} du téléphone connecté");
    Ok(())
}

/// Pipeline du son de retour (repli hors macOS) : `interaudiosrc` (alimenté par cpal) → Opus →
/// `appsink`. Renvoie le pipeline et le récepteur des trames Opus (données + durée).
#[cfg(not(target_os = "macos"))]
fn build_return_pipeline(cfg: &Config) -> Result<(gst::Pipeline, mpsc::UnboundedReceiver<(bytes::Bytes, Duration)>)> {
    let pipeline = gst::Pipeline::with_name("phone-return");
    let src = gst::ElementFactory::make("interaudiosrc")
        .property("channel", crate::engine::RETURN_AUDIO_CHANNEL)
        .build()?;
    let convert = gst::ElementFactory::make("audioconvert").build()?;
    let resample = gst::ElementFactory::make("audioresample").build()?;
    let cf = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("audio/x-raw")
                .field("rate", 48000i32)
                .field("channels", 2i32)
                .build(),
        )
        .build()?;
    let enc = gst::ElementFactory::make("opusenc")
        .property("bitrate", cfg.server.return_audio_bitrate)
        .property_from_str("audio-type", "voice")
        .build()?;
    let opus_caps = gst::Caps::builder("audio/x-opus").build();
    let sink = gst_app::AppSink::builder()
        .caps(&opus_caps)
        .sync(false)
        .max_buffers(16)
        .drop(false)
        .build();

    let (tx, rx) = mpsc::unbounded_channel::<(bytes::Bytes, Duration)>();
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |s| {
                let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                if let Some(buf) = sample.buffer() {
                    let dur = buf
                        .duration()
                        .map(|d| Duration::from_nanos(d.nseconds()))
                        .unwrap_or(Duration::from_millis(20));
                    if let Ok(map) = buf.map_readable() {
                        let _ = tx.send((bytes::Bytes::copy_from_slice(map.as_slice()), dur));
                    }
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    let sink_el: gst::Element = sink.upcast();
    pipeline.add_many([&src, &convert, &resample, &cf, &enc, &sink_el])?;
    gst::Element::link_many([&src, &convert, &resample, &cf, &enc, &sink_el])?;
    Ok((pipeline, rx))
}

/// Écrit les trames Opus de retour sur la piste locale une fois le type de charge utile négocié
/// (repli hors macOS ; sur macOS l'encodage est fait directement par [`spawn_opus_return`]).
#[cfg(not(target_os = "macos"))]
fn spawn_return_writer(
    track: Arc<TrackLocalStaticSample>,
    sender: Arc<dyn RtpSender>,
    ssrc: u32,
    mut rx: mpsc::UnboundedReceiver<(bytes::Bytes, Duration)>,
    cancel: CancellationToken,
) {
    // Le type de charge utile n'est connu qu'après négociation : on le résout en tâche de fond.
    let pt_slot = Arc::new(StdMutex::new(None::<u8>));
    {
        let sender = sender.clone();
        let pt_slot = pt_slot.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            loop {
                if cancel.is_cancelled() {
                    return;
                }
                if let Ok(params) = sender.get_parameters().await {
                    if let Some(codec) = params.rtp_parameters.codecs.first() {
                        *pt_slot.lock().unwrap() = Some(codec.payload_type);
                        return;
                    }
                }
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        });
    }
    tokio::spawn(async move {
        while let Some((data, duration)) = rx.recv().await {
            if cancel.is_cancelled() {
                break;
            }
            // Tant que la négociation n'est pas finie, il n'y a rien à envoyer : on jette.
            let Some(pt) = *pt_slot.lock().unwrap() else {
                continue;
            };
            let sample = Sample {
                data,
                duration,
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
    });
}

/// Attend que le type de charge utile de l'émetteur soit négocié (après la réponse SDP).
#[cfg(target_os = "macos")]
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

/// macOS : décode l'audio Opus entrant du téléphone avec libopus et le pousse dans le mixeur
/// cpal (F32 stéréo 48 kHz). Les paquets sortent déjà ordonnés du jitter buffer webrtc-rs ;
/// un paquet RTP Opus = une trame Opus (RFC 7587), donc pas de réassemblage.
#[cfg(target_os = "macos")]
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
                            continue; // doublon ou paquet en retard déjà dissimulé
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

/// macOS : encode le retour de la carte (entrée cpal, F32 stéréo 48 kHz) en Opus avec libopus
/// et l'écrit sur la piste locale webrtc-rs, sans GStreamer. Cadence 20 ms.
#[cfg(target_os = "macos")]
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
    /// interceptors + jitter buffer, ajout des pistes et génération d'une offre SDP valide.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn offer_generation_smoke() {
        let runtime = runtime();

        let mut media_engine = MediaEngine::default();
        media_engine.register_default_codecs().unwrap();
        let registry = register_default_interceptors(Registry::new(), &mut media_engine).unwrap();
        let registry = registry.with(
            Slot::JitterBuffer,
            JitterBufferBuilder::new()
                .with_depth(Duration::from_millis(60))
                .build(),
        );
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

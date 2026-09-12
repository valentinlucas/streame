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
use crate::engine::{frame_appsink, Engine, PhoneStats, RtpStats, RETURN_AUDIO_CHANNEL};
use anyhow::{Context, Result};
use gst::prelude::*;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use rtc::ice::mdns::MulticastDnsMode;
use rtc::interceptor::{JitterBufferBuilder, Registry, Slot};
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::media_engine::MIME_TYPE_OPUS;
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind,
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
    RTCPeerConnectionIceEvent, RTCPeerConnectionState, RTCSessionDescription, SettingEngineBuilder,
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
    /// Pipeline de décodage des flux entrants (une branche appsrc→decodebin par piste).
    decode_pipeline: gst::Pipeline,
    /// Pipeline du son de retour (carte → Opus → piste locale webrtc-rs).
    return_pipeline: gst::Pipeline,
}

/// Gestionnaire d'événements de la `PeerConnection`.
#[derive(Clone)]
struct Handler {
    engine: Arc<Engine>,
    out: mpsc::UnboundedSender<ServerMsg>,
    decode_pipeline: gst::Pipeline,
    cancel: CancellationToken,
    name: String,
    /// Codec vidéo réellement négocié (rempli à l'arrivée de la piste), pour l'affichage /control.
    video_codec: Arc<StdMutex<String>>,
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
            RTCPeerConnectionState::Connected => self.engine.set_phone(Some(self.name.clone())),
            RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed => {
                self.engine.set_phone(None);
                self.cancel.cancel();
            }
            _ => {}
        }
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

        // appsrc RTP → decodebin (dépayloadage + décodage matériel), branché au moteur.
        let appsrc = gst_app::AppSrc::builder()
            .is_live(true)
            .format(gst::Format::Time)
            .do_timestamp(true)
            .build();
        let decode = match gst::ElementFactory::make("decodebin").build() {
            Ok(d) => d,
            Err(e) => {
                error!("decodebin indisponible : {e}");
                return;
            }
        };
        let appsrc_el: gst::Element = appsrc.clone().upcast();
        if let Err(e) = (|| -> Result<()> {
            self.decode_pipeline.add_many([&appsrc_el, &decode])?;
            appsrc_el.link(&decode)?;
            let engine = self.engine.clone();
            let pipe = self.decode_pipeline.clone();
            decode.connect_pad_added(move |_, dpad| {
                if let Err(e) = attach_decoded_branch(&pipe, dpad, &engine) {
                    error!("branche de décodage : {e:#}");
                }
            });
            appsrc_el.sync_state_with_parent()?;
            decode.sync_state_with_parent()?;
            Ok(())
        })() {
            error!("mise en place du décodage : {e:#}");
            return;
        }

        // Boucle de lecture RTP : les paquets sortent déjà ordonnés/lissés du jitter buffer.
        let media_owned = media.to_string();
        let encoding_owned = encoding.to_string();
        // Paramètres fmtp (packetization-mode, profile-level-id, sprop-parameter-sets…) : sans
        // eux, rtph264depay peut mal réassembler les paquets FU-A de l'iPhone. webrtcbin les
        // fournissait dans les caps ; on les reconstitue depuis la ligne fmtp négociée.
        let fmtp_owned = codec
            .as_ref()
            .map(|c| c.sdp_fmtp_line.clone())
            .unwrap_or_default();
        let cancel = self.cancel.clone();
        // Piste conservée pour l'envoi périodique de PLI (vidéo) ; la boucle ci-dessous consomme `track`.
        let pli_track = if media == "video" {
            Some(track.clone())
        } else {
            None
        };
        tokio::spawn(async move {
            let mut caps_set = false;
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
                        if !caps_set {
                            let mut cb = gst::Caps::builder("application/x-rtp")
                                .field("media", media_owned.as_str())
                                .field("encoding-name", encoding_owned.as_str())
                                .field("clock-rate", clock_rate as i32)
                                .field("payload", pkt.header.payload_type as i32);
                            for kv in fmtp_owned.split(';') {
                                if let Some((k, v)) = kv.split_once('=') {
                                    let (k, v) = (k.trim(), v.trim());
                                    if !k.is_empty() && !v.is_empty() {
                                        cb = cb.field(k, v);
                                    }
                                }
                            }
                            let caps = cb.build();
                            appsrc.set_caps(Some(&caps));
                            caps_set = true;
                            info!("piste {media_owned} : 1er paquet RTP (pt={}), caps appsrc = {caps}", pkt.header.payload_type);
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
        if let Some(pli_track) = pli_track {
            let cancel = self.cancel.clone();
            tokio::spawn(async move {
                let mut n = 0u32;
                loop {
                    let pli = PictureLossIndication {
                        sender_ssrc: 0,
                        media_ssrc,
                    };
                    if pli_track.write_rtcp(vec![Box::new(pli)]).await.is_err() {
                        break;
                    }
                    n += 1;
                    let wait = if n < 6 {
                        Duration::from_millis(400)
                    } else {
                        Duration::from_secs(3)
                    };
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        _ = tokio::time::sleep(wait) => {}
                    }
                }
            });
        }
    }
}

impl PhoneSession {
    pub async fn start(
        cfg: &Config,
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
        let decode_pipeline =
            gst::Pipeline::with_name(&format!("phone-{}", name.replace(' ', "_")));
        let video_codec = Arc::new(StdMutex::new(cfg.server.video_codec.to_uppercase()));
        let handler = Arc::new(Handler {
            engine: engine.clone(),
            out: out.clone(),
            decode_pipeline: decode_pipeline.clone(),
            cancel: cancel.clone(),
            name: name.clone(),
            video_codec: video_codec.clone(),
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
        pc.add_transceiver_from_kind(
            RtpCodecKind::Video,
            Some(RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Recvonly,
                ..Default::default()
            }),
        )
        .await
        .context("transceiver vidéo")?;

        // Pipeline du son de retour : carte → Opus → appsink → piste webrtc-rs.
        let (return_pipeline, ret_rx) = build_return_pipeline(cfg).context("pipeline de retour")?;
        spawn_return_writer(return_track, sender, ssrc, ret_rx, cancel.clone());

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

        // Surveillance des bus : sans elle, une erreur de decodebin (pas de décodeur, caps
        // incompatibles, pads non liés) passe totalement inaperçue.
        spawn_bus_watch(&decode_pipeline, format!("decode-{name}"));
        spawn_bus_watch(&return_pipeline, format!("return-{name}"));

        decode_pipeline
            .set_state(gst::State::Playing)
            .context("démarrage du pipeline de décodage")?;
        return_pipeline
            .set_state(gst::State::Playing)
            .context("démarrage du pipeline de retour")?;

        info!("session WebRTC (webrtc-rs) démarrée pour « {name} »");
        Ok(Arc::new(PhoneSession {
            name,
            cancel,
            pc,
            decode_pipeline,
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

    pub fn close(&self) {
        self.cancel.cancel();
        let _ = self.decode_pipeline.set_state(gst::State::Null);
        let _ = self.return_pipeline.set_state(gst::State::Null);
        // Débloque les threads de surveillance des bus (iter_timed).
        if let Some(b) = self.decode_pipeline.bus() {
            b.set_flushing(true);
        }
        if let Some(b) = self.return_pipeline.bus() {
            b.set_flushing(true);
        }
        // `close()` de la PeerConnection est asynchrone : on la lance sans l'attendre si un
        // runtime Tokio est disponible ; sinon le `Drop` de la PeerConnection fera le ménage.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let pc = self.pc.clone();
            handle.spawn(async move {
                let _ = pc.close().await;
            });
        }
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

/// Construit la branche de décodage à partir d'un pad `decodebin` (vidéo → GPU, audio → moteur).
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

/// Pipeline du son de retour : `interaudiosrc` (alimenté par cpal) → Opus → `appsink`.
/// Renvoie le pipeline et le récepteur des trames Opus (données + durée).
fn build_return_pipeline(cfg: &Config) -> Result<(gst::Pipeline, mpsc::UnboundedReceiver<(bytes::Bytes, Duration)>)> {
    let pipeline = gst::Pipeline::with_name("phone-return");
    let src = gst::ElementFactory::make("interaudiosrc")
        .property("channel", RETURN_AUDIO_CHANNEL)
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

/// Écrit les trames Opus de retour sur la piste locale une fois le type de charge utile négocié.
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

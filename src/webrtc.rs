//! Session WebRTC avec le téléphone (un pipeline GStreamer par téléphone).
//!
//! Le Mac est l'« offreur » : il propose un flux audio bidirectionnel (retour vers le
//! téléphone) et une réception vidéo. Les flux décodés sont poussés dans les canaux
//! `intervideosink` / `interaudiosink` consommés par le moteur principal.

use crate::config::Config;
use crate::engine::{frame_appsink, Engine, PHONE_AUDIO_CHANNEL, RETURN_AUDIO_CHANNEL};
use anyhow::{Context, Result};
use gst::prelude::*;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

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
}

pub struct PhoneSession {
    pub name: String,
    pipeline: gst::Pipeline,
    webrtc: gst::Element,
    pub cancel: CancellationToken,
}

impl PhoneSession {
    pub fn start(
        cfg: &Config,
        name: String,
        out: mpsc::UnboundedSender<ServerMsg>,
        engine: Arc<Engine>,
    ) -> Result<Arc<Self>> {
        let pipeline = gst::Pipeline::with_name(&format!("phone-{}", name.replace(' ', "_")));
        let mut wb = gst::ElementFactory::make("webrtcbin")
            .name("webrtc")
            .property_from_str("bundle-policy", "max-bundle")
            .property("latency", cfg.server.rtc_latency_ms);
        if !cfg.server.stun_server.trim().is_empty() {
            wb = wb.property("stun-server", cfg.server.stun_server.trim());
        }
        let webrtc = wb.build().context("webrtcbin indisponible")?;
        pipeline.add(&webrtc)?;

        // --- Audio retour : carte son → téléphone -------------------------------------------
        let src = gst::ElementFactory::make("interaudiosrc")
            .property("channel", RETURN_AUDIO_CHANNEL)
            .build()?;
        let convert = gst::ElementFactory::make("audioconvert").build()?;
        let resample = gst::ElementFactory::make("audioresample").build()?;
        let cf = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gst::Caps::builder("audio/x-raw")
                    .field("rate", 48000)
                    .field("channels", 2)
                    .build(),
            )
            .build()?;
        let enc = gst::ElementFactory::make("opusenc")
            .property("bitrate", cfg.server.return_audio_bitrate)
            .property_from_str("audio-type", "voice")
            .build()?;
        let pay = gst::ElementFactory::make("rtpopuspay")
            .property("pt", 96u32)
            .build()?;
        let q = gst::ElementFactory::make("queue").build()?;
        let rtp_caps = gst::Caps::builder("application/x-rtp")
            .field("media", "audio")
            .field("encoding-name", "OPUS")
            .field("payload", 96i32)
            .field("clock-rate", 48000i32)
            .build();
        let rtp_cf = gst::ElementFactory::make("capsfilter")
            .property("caps", &rtp_caps)
            .build()?;
        pipeline.add_many([&src, &convert, &resample, &cf, &enc, &pay, &q, &rtp_cf])?;
        gst::Element::link_many([&src, &convert, &resample, &cf, &enc, &pay, &q, &rtp_cf])?;
        let audio_sink_pad = webrtc
            .request_pad_simple("sink_%u")
            .context("pad audio webrtcbin")?;
        rtp_cf.static_pad("src").unwrap().link(&audio_sink_pad)?;
        let audio_trans =
            audio_sink_pad.property::<gst_webrtc::WebRTCRTPTransceiver>("transceiver");
        audio_trans.set_property(
            "direction",
            gst_webrtc::WebRTCRTPTransceiverDirection::Sendrecv,
        );

        // --- Vidéo : réception seule ----------------------------------------------------------
        let codec = cfg.server.video_codec.to_ascii_uppercase();
        let mut vb = gst::Caps::builder("application/x-rtp")
            .field("media", "video")
            .field("clock-rate", 90000i32)
            .field("rtcp-fb-nack-pli", true)
            .field("rtcp-fb-ccm-fir", true)
            .field("rtcp-fb-transport-cc", true);
        vb = if codec == "VP8" {
            vb.field("encoding-name", "VP8").field("payload", 97i32)
        } else {
            vb.field("encoding-name", "H264")
                .field("payload", 102i32)
                .field("packetization-mode", "1")
                .field(
                    "profile-level-id",
                    cfg.server.h264_profile_level_id.as_str(),
                )
                .field("level-asymmetry-allowed", "1")
        };
        let video_caps = vb.build();
        let video_trans = webrtc.emit_by_name::<gst_webrtc::WebRTCRTPTransceiver>(
            "add-transceiver",
            &[
                &gst_webrtc::WebRTCRTPTransceiverDirection::Recvonly,
                &video_caps,
            ],
        );
        video_trans.set_property("do-nack", true);

        let cancel = CancellationToken::new();
        let session = Arc::new(PhoneSession {
            name: name.clone(),
            pipeline: pipeline.clone(),
            webrtc: webrtc.clone(),
            cancel,
        });

        // --- Signaux webrtcbin ---------------------------------------------------------------
        {
            let out = out.clone();
            webrtc.connect("on-negotiation-needed", false, move |values| {
                let wb = values[0].get::<gst::Element>().expect("webrtcbin");
                let out = out.clone();
                let wb2 = wb.clone();
                let promise = gst::Promise::with_change_func(move |reply| {
                    let offer = match reply {
                        Ok(Some(s)) => s.get::<gst_webrtc::WebRTCSessionDescription>("offer"),
                        Ok(None) => {
                            error!("create-offer : réponse vide");
                            return;
                        }
                        Err(e) => {
                            error!("create-offer : {e:?}");
                            return;
                        }
                    };
                    let Ok(offer) = offer else {
                        error!("create-offer : pas d'offre dans la réponse");
                        return;
                    };
                    wb2.emit_by_name::<()>(
                        "set-local-description",
                        &[&offer, &None::<gst::Promise>],
                    );
                    match offer.sdp().as_text() {
                        Ok(sdp) => {
                            debug!("offre SDP envoyée");
                            let _ = out.send(ServerMsg::Offer { sdp });
                        }
                        Err(e) => error!("sdp → texte : {e}"),
                    }
                });
                wb.emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);
                None
            });
        }
        {
            let out = out.clone();
            webrtc.connect("on-ice-candidate", false, move |values| {
                let mline = values[1].get::<u32>().unwrap_or(0);
                let candidate = values[2].get::<String>().unwrap_or_default();
                let _ = out.send(ServerMsg::Ice {
                    candidate,
                    sdp_m_line_index: mline,
                });
                None
            });
        }
        {
            let engine = engine.clone();
            let name = name.clone();
            let cancel = session.cancel.clone();
            webrtc.connect_notify(Some("connection-state"), move |wb, _| {
                let state =
                    wb.property::<gst_webrtc::WebRTCPeerConnectionState>("connection-state");
                info!("téléphone « {name} » : état WebRTC {state:?}");
                match state {
                    gst_webrtc::WebRTCPeerConnectionState::Connected => {
                        engine.set_phone(Some(name.clone()))
                    }
                    gst_webrtc::WebRTCPeerConnectionState::Failed
                    | gst_webrtc::WebRTCPeerConnectionState::Closed => {
                        engine.set_phone(None);
                        cancel.cancel();
                    }
                    _ => {}
                }
            });
        }
        {
            let pipeline = pipeline.clone();
            let engine = engine.clone();
            webrtc.connect_pad_added(move |_, pad| {
                if pad.direction() != gst::PadDirection::Src {
                    return;
                }
                if let Err(e) = Self::on_incoming_stream(&pipeline, pad, &engine) {
                    error!("flux entrant : {e:#}");
                }
            });
        }

        // --- Bus ---------------------------------------------------------------------------
        {
            let bus = pipeline.bus().expect("bus");
            let cancel = session.cancel.clone();
            let name = name.clone();
            std::thread::Builder::new()
                .name(format!("bus-{name}"))
                .spawn(move || {
                    for msg in bus.iter_timed(gst::ClockTime::NONE) {
                        use gst::MessageView;
                        match msg.view() {
                            MessageView::Error(e) => {
                                error!("WebRTC « {name} » : {} ({:?})", e.error(), e.debug());
                                cancel.cancel();
                            }
                            MessageView::Warning(w) => warn!("WebRTC « {name} » : {}", w.error()),
                            MessageView::Eos(_) => {
                                info!("WebRTC « {name} » : fin de flux");
                                cancel.cancel();
                            }
                            _ => {}
                        }
                    }
                })
                .context("thread bus webrtc")?;
        }

        pipeline
            .set_state(gst::State::Playing)
            .context("démarrage du pipeline WebRTC")?;
        info!("session WebRTC démarrée pour « {name} » (codec {codec})");
        Ok(session)
    }

    /// Un flux RTP décodable arrive du téléphone : on le décode et on l'envoie au moteur.
    fn on_incoming_stream(
        pipeline: &gst::Pipeline,
        pad: &gst::Pad,
        engine: &Arc<Engine>,
    ) -> Result<()> {
        let decode = gst::ElementFactory::make("decodebin").build()?;
        pipeline.add(&decode)?;
        decode.sync_state_with_parent()?;
        pad.link(&decode.static_pad("sink").unwrap())?;
        let pipe = pipeline.clone();
        let engine = engine.clone();
        decode.connect_pad_added(move |_, dpad| {
            let Some(caps) = dpad.current_caps() else {
                return;
            };
            let Some(s) = caps.structure(0) else { return };
            let media = s.name().to_string();
            let res: Result<()> = (|| {
                let q = gst::ElementFactory::make("queue")
                    .property("max-size-buffers", 2u32)
                    .property("max-size-time", 0u64)
                    .property("max-size-bytes", 0u32)
                    .property_from_str("leaky", "downstream")
                    .build()?;
                let chain: Vec<gst::Element> = if media.starts_with("video/") {
                    // Décodé (vtdec → NV12) puis envoyé tel quel au GPU, sans synchronisation :
                    // le jitter buffer WebRTC a déjà lissé le flux, on affiche au plus tôt.
                    let conv = gst::ElementFactory::make("videoconvert").build()?;
                    let cf = gst::ElementFactory::make("capsfilter")
                        .property("caps", crate::engine::gpu_caps())
                        .build()?;
                    let sink = frame_appsink(engine.phone_slot().clone(), false);
                    vec![q, conv, cf, sink.upcast()]
                } else if media.starts_with("audio/") {
                    let conv = gst::ElementFactory::make("audioconvert").build()?;
                    let res = gst::ElementFactory::make("audioresample").build()?;
                    let sink = gst::ElementFactory::make("interaudiosink")
                        .property("channel", PHONE_AUDIO_CHANNEL)
                        .build()?;
                    vec![q, conv, res, sink]
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
            })();
            if let Err(e) = res {
                error!("branche de décodage {media} : {e:#}");
            }
        });
        Ok(())
    }

    pub fn set_answer(&self, sdp: &str) -> Result<()> {
        let msg =
            gst_sdp::SDPMessage::parse_buffer(sdp.as_bytes()).context("SDP de réponse invalide")?;
        let answer =
            gst_webrtc::WebRTCSessionDescription::new(gst_webrtc::WebRTCSDPType::Answer, msg);
        self.webrtc
            .emit_by_name::<()>("set-remote-description", &[&answer, &None::<gst::Promise>]);
        debug!("réponse SDP appliquée");
        Ok(())
    }

    pub fn add_ice(&self, mline: u32, candidate: &str) {
        self.webrtc
            .emit_by_name::<()>("add-ice-candidate", &[&mline, &candidate]);
    }

    pub fn close(&self) {
        self.cancel.cancel();
        let _ = self.pipeline.set_state(gst::State::Null);
        if let Some(bus) = self.pipeline.bus() {
            bus.set_flushing(true);
        }
    }
}

impl Drop for PhoneSession {
    fn drop(&mut self) {
        self.close();
    }
}

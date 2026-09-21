//! Audio Opus par **libopus**, sur des pistes webrtc-rs — le même code aux deux bouts :
//! le Mac encode le retour de la carte et décode le micro du téléphone ; l'iPhone encode son
//! micro et décode le retour.
//!
//! - L'encodage est **cadencé par la source** ([`PcmSource::pull_frame`]) : une trame Opus dès que
//!   20 ms ont été capturées, pas d'horloge murale, pas de trame complétée en silence → aucune
//!   dérive à compenser, les horodatages RTP suivent exactement la capture.
//! - Le décodage suit les numéros de séquence : sur un trou de 1 à 3 paquets les trames
//!   manquantes sont reconstituées par PLC et la dernière par le FEC en bande du paquet suivant.

use anyhow::{Context, Result};
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::media_engine::MIME_TYPE_OPUS;
use rtc::peer_connection::transport::RTCDtlsTransportState;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind,
};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::rtp_transceiver::RtpSender;

/// Fréquence d'échantillonnage sur les pistes (Opus travaille en interne à 48 kHz).
pub const SAMPLE_RATE: u32 = 48_000;
/// Une trame = 20 ms stéréo.
pub const FRAME_SAMPLES: usize = 960 * 2;

/// Source PCM pour l'encodeur : F32 stéréo entrelacé à 48 kHz.
pub trait PcmSource: Send + Sync {
    /// Extrait exactement une trame (`out.len()` échantillons) si elle est disponible et renvoie
    /// `true` ; sinon ne touche pas `out` et renvoie `false`. L'appelant attend alors
    /// [`PcmSource::notify`] (ou réessaie ~3 ms plus tard si la source n'en fournit pas).
    fn pull_frame(&self, out: &mut [f32]) -> bool;

    /// Signal (`notify_one`) émis par le producteur quand une trame est disponible : évite la
    /// scrutation. `None` = pas de signal, l'encodeur scrute.
    fn notify(&self) -> Option<Arc<tokio::sync::Notify>> {
        None
    }
}

/// Destination PCM du décodeur : F32 stéréo entrelacé à 48 kHz, une trame décodée à la fois.
pub trait PcmSink: Send + Sync {
    fn push(&self, samples: &[f32]);
}

impl<F: Fn(&[f32]) + Send + Sync> PcmSink for F {
    fn push(&self, samples: &[f32]) {
        self(samples)
    }
}

/// Description de codec de la piste Opus locale (stéréo 48 kHz).
pub fn codec() -> RTCRtpCodec {
    RTCRtpCodec {
        mime_type: MIME_TYPE_OPUS.to_owned(),
        clock_rate: SAMPLE_RATE,
        channels: 2,
        sdp_fmtp_line: String::new(),
        rtcp_feedback: vec![],
    }
}

/// Crée la piste Opus locale (à ajouter à la `PeerConnection`) et renvoie son SSRC.
pub fn new_local_track(
    stream_id: &str,
    track_id: &str,
    label: &str,
) -> Result<(Arc<TrackLocalStaticSample>, u32)> {
    let ssrc = rand::random::<u32>();
    let track = TrackLocalStaticSample::new(
        Instant::now(),
        MediaStreamTrack::new(
            stream_id.to_string(),
            track_id.to_string(),
            label.to_string(),
            RtpCodecKind::Audio,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    ssrc: Some(ssrc),
                    ..Default::default()
                },
                codec: codec(),
                ..Default::default()
            }],
        ),
    )
    .context("piste Opus locale")?;
    Ok((Arc::new(track), ssrc))
}

/// Attend que l'émetteur soit prêt à écrire : type de charge utile négocié (après l'échange
/// SDP) **et** transport DTLS établi — écrire avant, c'est jeter des paquets (« local SRTP
/// context is not set yet »). Renvoie le type de charge utile.
pub async fn resolve_payload_type(
    sender: &Arc<dyn RtpSender>,
    cancel: &CancellationToken,
) -> Option<u8> {
    let mut pt: Option<u8> = None;
    loop {
        if cancel.is_cancelled() {
            return None;
        }
        if pt.is_none() {
            if let Ok(params) = sender.get_parameters().await {
                pt = params.rtp_parameters.codecs.first().map(|c| c.payload_type);
            }
        }
        if pt.is_some() {
            let dtls_ok = match sender.transport().await {
                Ok(Some(t)) => matches!(t.state().await, Ok(RTCDtlsTransportState::Connected)),
                _ => false,
            };
            if dtls_ok {
                return pt;
            }
        }
        tokio::select! {
            _ = cancel.cancelled() => return None,
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
    }
}

/// Réglages de l'encodeur.
#[derive(Debug, Clone, Copy)]
pub struct EncoderOptions {
    /// Débit cible (bit/s).
    pub bitrate: i32,
    /// `Voip` (retour d'oreillette, parole) ou `Audio` (micro caméra, fidélité).
    pub application: opus::Application,
    /// Nom pour les journaux.
    pub label: &'static str,
}

/// Encode la source (F32 stéréo 48 kHz) en Opus avec libopus et l'écrit sur la piste locale.
/// Cadence 20 ms dictée par la source ; FEC en bande activé (un paquet perdu isolé est récupéré
/// par le récepteur sans retransmission).
pub fn spawn_encoder(
    source: Arc<dyn PcmSource>,
    track: Arc<TrackLocalStaticSample>,
    sender: Arc<dyn RtpSender>,
    ssrc: u32,
    opts: EncoderOptions,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        let Some(pt) = resolve_payload_type(&sender, &cancel).await else {
            return;
        };
        let mut enc = match opus::Encoder::new(SAMPLE_RATE, opus::Channels::Stereo, opts.application)
        {
            Ok(e) => e,
            Err(e) => {
                error!("encodeur Opus ({}) : {e}", opts.label);
                return;
            }
        };
        let _ = enc.set_bitrate(opus::Bitrate::Bits(opts.bitrate));
        let _ = enc.set_inband_fec(true);
        let _ = enc.set_packet_loss_perc(10);
        let _ = enc.set_dtx(false);
        let mut pcm = vec![0f32; FRAME_SAMPLES];
        let mut out = vec![0u8; 4000];
        info!("{} : encodage Opus (libopus) démarré, cadencé par la source", opts.label);
        loop {
            if cancel.is_cancelled() {
                break;
            }
            if !source.pull_frame(&mut pcm) {
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
                Err(e) => warn!("encodage Opus ({}) : {e}", opts.label),
            }
        }
    });
}

/// Décode la piste Opus distante avec libopus et pousse le PCM (F32 stéréo 48 kHz) dans le
/// puits. Un paquet RTP Opus = une trame (RFC 7587), donc pas de réassemblage. Sans jitter
/// buffer paquet, un paquet arrivé dans le désordre est ignoré : sa trame a déjà été dissimulée.
pub fn spawn_decoder(
    track: Arc<dyn TrackRemote>,
    sink: Arc<dyn PcmSink>,
    label: &'static str,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        let mut dec = match opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo) {
            Ok(d) => d,
            Err(e) => {
                error!("décodeur Opus ({label}) : {e}");
                return;
            }
        };
        let mut pcm = vec![0f32; 5760 * 2]; // jusqu'à 120 ms stéréo @ 48 kHz
        let mut started = false;
        let mut last_seq: Option<u16> = None;
        let mut last_per_ch: usize = 960;
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
                                    sink.push(&pcm[..n * 2]);
                                }
                            }
                            if let Ok(n) = dec.decode_float(payload, &mut pcm[..fs], true) {
                                sink.push(&pcm[..n * 2]);
                            }
                            concealed += lost as u64;
                            if concealed % 50 < lost as u64 {
                                debug!("{label} : {concealed} trames dissimulées (PLC/FEC)");
                            }
                        }
                    }
                    last_seq = Some(seq);
                    match dec.decode_float(payload, &mut pcm, false) {
                        Ok(per_ch) => {
                            last_per_ch = per_ch.max(1);
                            sink.push(&pcm[..per_ch * 2]);
                            if !started {
                                info!("{label} : décodage Opus (libopus) démarré");
                                started = true;
                            }
                        }
                        Err(e) => warn!("décodage Opus ({label}) : {e}"),
                    }
                }
                TrackRemoteEvent::OnEnded | TrackRemoteEvent::OnError => break,
                _ => {}
            }
        }
    });
}

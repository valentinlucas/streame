//! H264 : paramètres de codec négociés (`packetization-mode=1`, celui des iPhone), découpe
//! Annex-B, débit annoncé/plafonné. Le décodage (Mac) et l'encodage (iPhone) sont faits par
//! VideoToolbox de part et d'autre.

use rtc::peer_connection::configuration::media_engine::MIME_TYPE_H264;
use rtc::rtp_transceiver::rtp_sender::{RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters};

/// Retours RTCP déclarés sur H264 (les mêmes que `register_default_codecs`).
pub fn rtcp_feedback() -> Vec<RTCPFeedback> {
    vec![
        RTCPFeedback { typ: "goog-remb".into(), parameter: String::new() },
        RTCPFeedback { typ: "ccm".into(), parameter: "fir".into() },
        RTCPFeedback { typ: "nack".into(), parameter: String::new() },
        RTCPFeedback { typ: "nack".into(), parameter: "pli".into() },
        RTCPFeedback { typ: "transport-cc".into(), parameter: String::new() },
    ]
}

/// Ligne fmtp H264 standard pour un `profile-level-id` donné.
pub fn fmtp(profile_level_id: &str) -> String {
    format!("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id={profile_level_id}")
}

/// Description de codec H264 (90 kHz) avec la ligne fmtp donnée.
pub fn codec(sdp_fmtp_line: &str) -> RTCRtpCodec {
    RTCRtpCodec {
        mime_type: MIME_TYPE_H264.to_owned(),
        clock_rate: 90000,
        channels: 0,
        sdp_fmtp_line: sdp_fmtp_line.to_owned(),
        rtcp_feedback: rtcp_feedback(),
    }
}

/// Codecs vidéo offerts par le Mac : **H264 uniquement**, en `packetization-mode=1`, le profil
/// configuré en tête. Le décodage est matériel (VideoToolbox), qui ne gère ni VP8 ni VP9 : les
/// proposer reviendrait à risquer une négociation sans image. Les paramètres (fmtp, types de
/// charge utile) reprennent ceux enregistrés par `register_default_codecs`, sinon la préférence
/// est refusée.
pub fn video_codec_preferences(profile_level_id: &str) -> Vec<RTCRtpCodecParameters> {
    let params = |fmtp: &str, pt: u8| RTCRtpCodecParameters {
        rtp_codec: codec(fmtp),
        payload_type: pt,
    };
    let mut out = vec![
        params("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f", 125),
        params("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f", 102),
        params("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640032", 123),
    ];
    let wanted = profile_level_id.trim().to_ascii_lowercase();
    if let Some(i) = out
        .iter()
        .position(|c| c.rtp_codec.sdp_fmtp_line.contains(&format!("profile-level-id={wanted}")))
    {
        let first = out.remove(i);
        out.insert(0, first);
    }
    out
}

/// Ajoute `x-google-start-bitrate=<kb/s>` aux lignes fmtp H264 de l'offre **envoyée au
/// téléphone**. libwebrtc (Chrome, Safari) lit ce paramètre dans la description distante et
/// démarre son encodeur à ce débit au lieu de 300 kb/s. L'app iOS le lit aussi. Seul le texte
/// transmis est modifié. `kbps` = 0 → texte inchangé.
pub fn announce_start_bitrate(sdp: &str, kbps: u32) -> String {
    if kbps == 0 {
        return sdp.to_string();
    }
    let mut out = String::with_capacity(sdp.len() + 64);
    for line in sdp.split_inclusive('\n') {
        let body = line.trim_end_matches(['\r', '\n']);
        if body.starts_with("a=fmtp:")
            && body.contains("profile-level-id=")
            && !body.contains("x-google-start-bitrate=")
        {
            out.push_str(body);
            out.push_str(&format!(";x-google-start-bitrate={kbps}"));
            out.push_str(&line[body.len()..]);
        } else {
            out.push_str(line);
        }
    }
    out
}

/// Paramètres H264 lus dans une offre SDP : premier `a=fmtp` H264 en `packetization-mode=1`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferedH264 {
    pub payload_type: u8,
    pub profile_level_id: String,
    /// Ligne fmtp complète (sans `x-google-start-bitrate`, propre à l'offreur).
    pub fmtp: String,
    /// `x-google-start-bitrate` (kb/s) si annoncé.
    pub start_bitrate_kbps: Option<u32>,
}

/// Cherche le H264 `packetization-mode=1` préféré (le premier) de l'offre.
pub fn offered_h264(sdp: &str) -> Option<OfferedH264> {
    // pt → nom de codec (rtpmap), pour ne retenir que les fmtp H264.
    let mut h264_pts: Vec<u8> = Vec::new();
    for line in sdp.lines() {
        let l = line.trim_end();
        if let Some(rest) = l.strip_prefix("a=rtpmap:") {
            let mut it = rest.splitn(2, ' ');
            let pt = it.next().and_then(|p| p.parse::<u8>().ok());
            let name = it.next().unwrap_or("");
            if let Some(pt) = pt {
                if name.to_ascii_uppercase().starts_with("H264/") {
                    h264_pts.push(pt);
                }
            }
        }
    }
    let mut best: Option<(usize, OfferedH264)> = None;
    for line in sdp.lines() {
        let l = line.trim_end();
        let Some(rest) = l.strip_prefix("a=fmtp:") else { continue };
        let mut it = rest.splitn(2, ' ');
        let Some(pt) = it.next().and_then(|p| p.parse::<u8>().ok()) else { continue };
        let Some(rank) = h264_pts.iter().position(|p| *p == pt) else { continue };
        let params = it.next().unwrap_or("");
        let mut profile = None;
        let mut pm1 = false;
        let mut start = None;
        let mut kept: Vec<&str> = Vec::new();
        for kv in params.split(';') {
            let kv = kv.trim();
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            match k {
                "profile-level-id" => profile = Some(v.to_ascii_lowercase()),
                "packetization-mode" => pm1 = v == "1",
                "x-google-start-bitrate" => start = v.parse().ok(),
                _ => {}
            }
            if k != "x-google-start-bitrate" && !kv.is_empty() {
                kept.push(kv);
            }
        }
        let (Some(profile), true) = (profile, pm1) else { continue };
        let cand = OfferedH264 {
            payload_type: pt,
            profile_level_id: profile,
            fmtp: kept.join(";"),
            start_bitrate_kbps: start,
        };
        if best.as_ref().map(|(r, _)| rank < *r).unwrap_or(true) {
            best = Some((rank, cand));
        }
    }
    best.map(|(_, c)| c)
}

/// Plafond de débit (kb/s) selon la hauteur d'image : la valeur configurée vaut pour 1080p, les
/// autres résolutions en prennent une part proportionnelle au nombre de pixels (même règle que
/// la page web).
pub fn max_bitrate_kbps(height: u32, max_1080_kbps: u32) -> u32 {
    let share = match height {
        h if h >= 1080 => 1.0,
        h if h >= 720 => 0.5625,
        h if h >= 480 => 0.25,
        _ => 0.5,
    };
    (max_1080_kbps as f64 * share).round() as u32
}

/// Découpe un flux Annex-B en NALUs (sans les codes de démarrage).
pub fn nalus(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut start: Option<usize> = None;
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if let Some(s) = start {
                let mut end = i;
                if end > s && data[end - 1] == 0 {
                    end -= 1; // code à 4 octets (00 00 00 01)
                }
                if end > s {
                    out.push(&data[s..end]);
                }
            }
            i += 3;
            start = Some(i);
        } else {
            i += 1;
        }
    }
    if let Some(s) = start {
        if s < data.len() {
            out.push(&data[s..]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debit_de_depart_annonce_sur_les_fmtp_h264() {
        let sdp = "m=video 9 UDP/TLS/RTP/SAVPF 125 103\r\n\
                   a=fmtp:125 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n\
                   a=fmtp:103 apt=125\r\n\
                   a=fmtp:111 minptime=10;useinbandfec=1\r\n";
        let out = announce_start_bitrate(sdp, 3000);
        assert!(out.contains("profile-level-id=42e01f;x-google-start-bitrate=3000\r\n"));
        assert!(out.contains("a=fmtp:103 apt=125\r\n"), "RTX inchangé");
        assert!(out.contains("a=fmtp:111 minptime=10;useinbandfec=1\r\n"), "Opus inchangé");
        assert_eq!(announce_start_bitrate(sdp, 0), sdp);
        assert_eq!(announce_start_bitrate(&out, 3000), out, "idempotent");
    }

    #[test]
    fn lecture_du_h264_offert() {
        let sdp = "m=video 9 UDP/TLS/RTP/SAVPF 102 125\r\n\
                   a=rtpmap:102 H264/90000\r\n\
                   a=rtpmap:125 H264/90000\r\n\
                   a=rtpmap:111 opus/48000/2\r\n\
                   a=fmtp:111 minptime=10;useinbandfec=1\r\n\
                   a=fmtp:125 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f;x-google-start-bitrate=3000\r\n\
                   a=fmtp:102 level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42001f\r\n";
        let got = offered_h264(sdp).unwrap();
        assert_eq!(got.payload_type, 125, "seul le packetization-mode=1 compte");
        assert_eq!(got.profile_level_id, "42e01f");
        assert_eq!(got.fmtp, "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f");
        assert_eq!(got.start_bitrate_kbps, Some(3000));
        assert!(offered_h264("m=audio 9\r\n").is_none());
    }

    #[test]
    fn plafond_par_resolution() {
        assert_eq!(max_bitrate_kbps(1080, 8000), 8000);
        assert_eq!(max_bitrate_kbps(720, 8000), 4500);
        assert_eq!(max_bitrate_kbps(480, 8000), 2000);
    }

    #[test]
    fn decoupe_annexb() {
        let data = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4, 5, 6];
        let n = nalus(&data);
        assert_eq!(n.len(), 3);
        assert_eq!(n[0], &[0x67, 1, 2]);
        assert_eq!(n[1], &[0x68, 3]);
        assert_eq!(n[2], &[0x65, 4, 5, 6]);
    }
}

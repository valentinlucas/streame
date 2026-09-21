//! Protocole de signaling entre le téléphone et la régie (JSON sur WebSocket `/ws`).
//!
//! Le Mac fait l'**offre** ; le téléphone répond. Les deux enums sont (dé)sérialisables dans les
//! deux sens : le Mac lit `ClientMsg` et écrit `ServerMsg`, le téléphone l'inverse.

use serde::{Deserialize, Serialize};

/// Statistiques locales du téléphone (encodeur, réseau), envoyées chaque seconde.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PhoneStats {
    pub width: u32,
    pub height: u32,
    pub fps: f32,
    pub bitrate_kbps: f32,
    pub quality_limitation: String,
    pub rtt_ms: Option<f32>,
    pub codec: String,
}

/// Messages téléphone → serveur.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello {
        #[serde(default, skip_serializing_if = "Option::is_none")]
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
    /// Statistiques locales de la page ou de l'app (encodeur, réseau).
    Stats(PhoneStats),
    Bye,
}

/// Messages serveur → téléphone.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// Réglages publiés par la régie sur `GET /api/config`, lus par la page et par l'app.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Plafond de débit vidéo en 1080p (kb/s) ; les autres résolutions en prennent une part
    /// proportionnelle au nombre de pixels (voir [`crate::h264::max_bitrate_kbps`]).
    pub video_max_bitrate_kbps: u32,
    /// Débit de départ souhaité (kb/s), 0 = défaut.
    pub video_start_bitrate_kbps: u32,
    /// Serveur STUN (`stun:hôte:port`), vide sur un réseau local.
    pub stun_server: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            video_max_bitrate_kbps: 8000,
            video_start_bitrate_kbps: 3000,
            stun_server: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aller_retour_json_identique_a_la_page() {
        let hello: ClientMsg = serde_json::from_str(r#"{"type":"hello","name":"iPhone"}"#).unwrap();
        assert!(matches!(hello, ClientMsg::Hello { name: Some(ref n) } if n == "iPhone"));
        assert_eq!(
            serde_json::to_string(&ClientMsg::Hello { name: None }).unwrap(),
            r#"{"type":"hello"}"#
        );
        let ice = ClientMsg::Ice { candidate: "c".into(), sdp_m_line_index: 1 };
        assert_eq!(
            serde_json::to_string(&ice).unwrap(),
            r#"{"type":"ice","candidate":"c","sdpMLineIndex":1}"#
        );
        let on_air: ServerMsg = serde_json::from_str(r#"{"type":"on_air","on":true}"#).unwrap();
        assert!(matches!(on_air, ServerMsg::OnAir { on: true }));
        let stats: ClientMsg = serde_json::from_str(
            r#"{"type":"stats","width":1920,"height":1080,"fps":30,"bitrate_kbps":4000.5,"quality_limitation":"none","rtt_ms":null,"codec":"H264"}"#,
        )
        .unwrap();
        assert!(matches!(stats, ClientMsg::Stats(PhoneStats { width: 1920, .. })));
    }
}

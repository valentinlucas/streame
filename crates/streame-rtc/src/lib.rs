//! Cœur média de Streame, **partagé entre la régie Mac et l'app iOS**.
//!
//! Même pile des deux côtés : signaling JSON sur WebSocket, transport WebRTC par **webrtc-rs**
//! (ICE/DTLS/SRTP/RTP, NACK, TWCC), audio Opus par **libopus**, vidéo H264 (VideoToolbox), audio
//! par **cpal** (CoreAudio). Pas de GStreamer, pas de libwebrtc.
//!
//! - [`signaling`] : les messages échangés (`hello`/`offer`/`answer`/`ice`/`stats`/`on_air`/`bye`).
//! - [`pc`] : runtime webrtc-rs partagé, serveurs ICE, construction d'une `PeerConnection`.
//! - [`opus`] : piste Opus locale, tâches d'encodage (cadencées par la source) et de décodage
//!   (PLC/FEC) — les mêmes sur le Mac (retour / micro du téléphone) et sur l'iPhone (micro /
//!   retour du Mac).
//! - [`h264`] : paramètres de codec H264 (`packetization-mode=1`), découpe Annex-B, débit annoncé.
//! - [`client`] (feature `client`) : la session « téléphone » complète (répondeur WebRTC,
//!   encodeur VideoToolbox, audio cpal), utilisée par l'app iOS via `streame-ios` et par le banc
//!   `examples/fake_phone.rs` sur Mac.

pub mod h264;
pub mod opus;
pub mod pc;
pub mod signaling;

#[cfg(feature = "client")]
pub mod client;

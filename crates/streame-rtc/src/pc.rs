//! Construction d'une `PeerConnection` webrtc-rs, commune aux deux extrémités.

use anyhow::{Context, Result};
use rtc::ice::mdns::MulticastDnsMode;
use rtc::interceptor::Registry;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use webrtc::peer_connection::{
    register_default_interceptors, MediaEngine, PeerConnection, PeerConnectionBuilder,
    PeerConnectionEventHandler, RTCConfigurationBuilder, RTCIceServer, SettingEngineBuilder,
};
use webrtc::runtime::{default_runtime, Runtime};

/// Runtime webrtc-rs partagé (adossé à Tokio via la feature `runtime-tokio`).
///
/// `default_runtime()` construit un handle à chaque appel ; on le résout une seule fois pour
/// tout le process, comme le recommande la doc de webrtc-rs.
pub fn runtime() -> Arc<dyn Runtime> {
    static RT: OnceLock<Arc<dyn Runtime>> = OnceLock::new();
    RT.get_or_init(|| default_runtime().expect("runtime webrtc-rs (feature runtime-tokio)"))
        .clone()
}

/// Serveurs ICE à partir de la config (`stun_server`, vide = aucun sur un réseau local).
/// webrtc-rs attend la forme RFC 7064 « stun:hôte:port » ; l'ancienne forme « stun://… » de
/// webrtcbin est normalisée.
pub fn ice_servers(stun_server: &str) -> Vec<RTCIceServer> {
    let stun = stun_server.trim();
    if stun.is_empty() {
        return vec![];
    }
    vec![RTCIceServer {
        urls: vec![stun.replacen("://", ":", 1)],
        ..Default::default()
    }]
}

/// Media engine avec les codecs par défaut et les intercepteurs par défaut (NACK, rapports
/// RTCP, TWCC côté réception). `customize` permet d'ajouter des intercepteurs **avant** ceux
/// par défaut (contrôle de congestion GCC côté émetteur, par exemple).
pub fn media_engine_and_registry(
    customize: impl FnOnce(Registry, &mut MediaEngine) -> Result<Registry>,
) -> Result<(MediaEngine, Registry)> {
    let mut media_engine = MediaEngine::default();
    media_engine
        .register_default_codecs()
        .context("register_default_codecs")?;
    let registry = customize(Registry::new(), &mut media_engine)?;
    let registry = register_default_interceptors(registry, &mut media_engine)
        .context("interceptors par défaut")?;
    Ok((media_engine, registry))
}

/// Construit la `PeerConnection` : sockets UDP sur `udp_addrs` (`0.0.0.0:0` = une par interface,
/// candidats d'hôte exploitables sur le LAN ; ou des adresses précises, par ex. le Wi-Fi seul
/// sur iPhone), runtime partagé, mode mDNS au choix.
///
/// - Le Mac utilise `QueryOnly` : iOS/Safari masque ses candidats d'hôte derrière des noms
///   `.local` qu'il faut résoudre, sans annoncer les nôtres.
/// - L'app iOS (webrtc-rs des deux côtés) n'a pas besoin de mDNS : `Disabled`.
/// Adresses de liaison ICE limitées aux interfaces dont le nom commence par `prefix`
/// (`en` = Wi-Fi/Ethernet sur iOS et macOS : pas de cellulaire, ni de VPN, ni d'AWDL), IPv4
/// hors boucle locale et lien-local. Vide si aucune ne convient (repli sur `0.0.0.0:0`).
pub fn bind_addrs_for_interfaces(prefix: &str) -> Vec<String> {
    let Ok(list) = rtc::shared::ifaces::ifaces() else { return vec![] };
    let mut out: Vec<String> = Vec::new();
    for iface in list {
        let Some(addr) = iface.addr else { continue };
        let std::net::IpAddr::V4(ip) = addr.ip() else { continue };
        if !iface.name.starts_with(prefix) || ip.is_loopback() || ip.is_link_local() || ip.is_unspecified() {
            continue;
        }
        let s = format!("{ip}:0");
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

pub async fn build_peer_connection(
    ice_servers: Vec<RTCIceServer>,
    media_engine: MediaEngine,
    registry: Registry,
    mdns: MulticastDnsMode,
    udp_addrs: Vec<String>,
    handler: Arc<dyn PeerConnectionEventHandler>,
) -> Result<Arc<dyn PeerConnection>> {
    let config = RTCConfigurationBuilder::new()
        .with_ice_servers(ice_servers)
        .build();
    let setting_engine = SettingEngineBuilder::new()
        .with_multicast_dns_mode(mdns)
        .with_multicast_dns_timeout(Some(Duration::from_secs(5)))
        .build();
    let pc = PeerConnectionBuilder::new()
        .with_configuration(config)
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_setting_engine(setting_engine)
        .with_handler(handler)
        .with_runtime(runtime())
        .with_udp_addrs(udp_addrs)
        .build()
        .await
        .context("construction de la PeerConnection")?;
    Ok(Arc::new(pc))
}

//! Annonce Bonjour (mDNS-SD) de la régie : service `_streame._tcp` sur le port HTTPS, pour
//! que l'app iOS trouve le Mac sans saisir d'adresse (`NWBrowser`). Le nom d'instance est
//! `server.name` ou, à défaut, le nom de la machine.

use crate::config::Config;
use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceInfo};
use tracing::{info, warn};

pub const SERVICE_TYPE: &str = "_streame._tcp.local.";

/// Garde l'annonce vivante ; l'arrêt du démon la retire.
pub struct Advertiser {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Advertiser {
    pub fn start(cfg: &Config, ips: &[String], port: u16) -> Result<Self> {
        let daemon = ServiceDaemon::new().context("démon mDNS")?;
        // Première étiquette seulement : un nom de machine comme « val-macbook-pro.home »
        // donnerait « val-macbook-pro.home.local. », que les résolveurs mDNS (iOS) ne
        // résolvent pas ; les noms mDNS sont « étiquette.local. ».
        let host = hostname::get()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "streame".into());
        let host = host
            .split('.')
            .next()
            .filter(|l| !l.is_empty())
            .unwrap_or("streame")
            .to_string();
        let name = if cfg.server.name.trim().is_empty() {
            host.clone()
        } else {
            cfg.server.name.trim().to_string()
        };
        let addrs: Vec<std::net::IpAddr> = ips.iter().filter_map(|s| s.parse().ok()).collect();
        let props = [("path", "/ws"), ("version", env!("CARGO_PKG_VERSION"))];
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            &name,
            &format!("{host}.local."),
            &addrs[..],
            port,
            &props[..],
        )
        .context("description du service Bonjour")?;
        let fullname = info.get_fullname().to_string();
        daemon.register(info).context("enregistrement Bonjour")?;
        info!("Bonjour : « {name} » annoncé ({SERVICE_TYPE} port {port})");
        Ok(Self { daemon, fullname })
    }
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        if let Ok(rx) = self.daemon.unregister(&self.fullname) {
            let _ = rx.recv_timeout(std::time::Duration::from_millis(500));
        }
        if let Err(e) = self.daemon.shutdown() {
            warn!("arrêt du démon mDNS : {e}");
        }
    }
}

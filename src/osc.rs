//! Pilotage par OSC (UDP) : QLab (cues Réseau), Bitfocus Companion, TouchOSC…
//!
//! Adresses reconnues (mêmes actions que l'API HTTP) :
//! - `/streame/program <id>` ou `/streame/program/<id>` : passage à l'antenne avec transition ;
//! - `/streame/cut <id>` ou `/streame/cut/<id>` : bascule immédiate ;
//! - `/streame/preview <id>` ou `/streame/preview/<id>` : preview ;
//! - `/streame/take` : preview → programme.
//!
//! Une scène déjà à l'antenne redemandée relance ses vidéos pilotées par l'antenne (générique).
//! Les paquets groupés (bundles) sont acceptés. Pas de réponse : QLab n'en attend pas.

use crate::config::Config;
use crate::engine::Engine;
use rosc::{OscMessage, OscPacket, OscType};
use std::net::UdpSocket;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use tracing::{error, info, warn};

/// Action décodée d'un message OSC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Program(String),
    Cut(String),
    Preview(String),
    Take,
}

/// Décode un message : `/streame/<verbe>[/<id>]` avec l'identifiant en fin d'adresse ou en
/// premier argument (chaîne, ou entier = index de scène à partir de 1).
pub fn parse(msg: &OscMessage) -> Option<Action> {
    let mut parts = msg.addr.trim_matches('/').splitn(3, '/');
    if parts.next()? != "streame" {
        return None;
    }
    let verb = parts.next()?;
    let mut id = parts.next().map(|s| s.to_string());
    if id.is_none() {
        id = msg.args.first().and_then(|a| match a {
            OscType::String(s) => Some(s.clone()),
            OscType::Int(i) => Some(format!("#{i}")),
            OscType::Long(i) => Some(format!("#{i}")),
            _ => None,
        });
    }
    match (verb, id) {
        ("take", _) => Some(Action::Take),
        ("program", Some(id)) => Some(Action::Program(id)),
        ("cut", Some(id)) => Some(Action::Cut(id)),
        ("preview", Some(id)) => Some(Action::Preview(id)),
        _ => None,
    }
}

/// Identifiant de scène : `#n` = n-ième scène (1-based), sinon l'identifiant configuré.
fn scene_index(engine: &Engine, id: &str) -> Option<usize> {
    if let Some(n) = id.strip_prefix('#') {
        let n: usize = n.parse().ok()?;
        return (n >= 1 && n <= engine.scenes_ref().len()).then(|| n - 1);
    }
    engine.scene_index(id)
}

fn apply(engine: &Engine, action: Action) -> bool {
    match action {
        Action::Program(id) => scene_index(engine, &id)
            .map(|i| engine.transition_to(i))
            .is_some(),
        Action::Cut(id) => scene_index(engine, &id).map(|i| engine.cut(i)).is_some(),
        Action::Preview(id) => scene_index(engine, &id)
            .map(|i| engine.set_preview(i))
            .is_some(),
        Action::Take => {
            engine.take();
            true
        }
    }
}

fn handle(engine: &Engine, packet: OscPacket, from: &str) {
    match packet {
        OscPacket::Message(msg) => match parse(&msg) {
            Some(action) => {
                info!("OSC {from} : {} → {action:?}", msg.addr);
                if !apply(engine, action) {
                    warn!(
                        "OSC {from} : scène inconnue dans {} {:?}",
                        msg.addr, msg.args
                    );
                }
            }
            None => warn!("OSC {from} : message ignoré {} {:?}", msg.addr, msg.args),
        },
        OscPacket::Bundle(b) => {
            for p in b.content {
                handle(engine, p, from);
            }
        }
    }
}

/// Démarre le récepteur OSC (thread bloquant sur le socket UDP).
pub fn spawn(cfg: Arc<Config>, engine: Arc<Engine>) -> Option<JoinHandle<()>> {
    if !cfg.osc.enabled {
        return None;
    }
    let socket = match UdpSocket::bind(&cfg.osc.bind) {
        Ok(s) => s,
        Err(e) => {
            error!("OSC : écoute impossible sur {} : {e}", cfg.osc.bind);
            return None;
        }
    };
    info!(
        "OSC : écoute sur udp://{} (/streame/program <id>, /streame/take…)",
        cfg.osc.bind
    );
    thread::Builder::new()
        .name("osc".into())
        .spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                let (n, from) = match socket.recv_from(&mut buf) {
                    Ok(x) => x,
                    Err(e) => {
                        error!("OSC : réception : {e}");
                        thread::sleep(std::time::Duration::from_millis(200));
                        continue;
                    }
                };
                match rosc::decoder::decode_udp(&buf[..n]) {
                    Ok((_, packet)) => handle(&engine, packet, &from.to_string()),
                    Err(e) => warn!("OSC : paquet invalide de {from} : {e:?}"),
                }
            }
        })
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(addr: &str, args: Vec<OscType>) -> OscMessage {
        OscMessage {
            addr: addr.into(),
            args,
        }
    }

    #[test]
    fn adresses() {
        assert_eq!(
            parse(&msg(
                "/streame/program",
                vec![OscType::String("live".into())]
            )),
            Some(Action::Program("live".into()))
        );
        assert_eq!(
            parse(&msg("/streame/program/live", vec![])),
            Some(Action::Program("live".into()))
        );
        assert_eq!(
            parse(&msg("/streame/cut", vec![OscType::Int(2)])),
            Some(Action::Cut("#2".into()))
        );
        assert_eq!(
            parse(&msg("/streame/preview/black", vec![OscType::Int(9)])),
            Some(Action::Preview("black".into()))
        );
        assert_eq!(parse(&msg("/streame/take", vec![])), Some(Action::Take));
        assert_eq!(parse(&msg("/streame/program", vec![])), None);
        assert_eq!(parse(&msg("/qlab/go", vec![])), None);
        assert_eq!(parse(&msg("/streame/nope/live", vec![])), None);
    }
}

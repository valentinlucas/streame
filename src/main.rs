//! Streame : régie vidéo légère pour Mac.
//!
//! Un téléphone se connecte en HTTPS, envoie sa caméra et son micro en WebRTC et reçoit
//! un retour audio. Le Mac compose des scènes (façon OBS), affiche le programme sur la
//! sortie HDMI, route l'audio vers une carte multicanal (Behringer Wing) et se pilote au
//! Stream Deck, à la souris (multiview) ou depuis une page web.

mod audio;
mod avf;
mod config;
mod engine;
mod frame;
mod layout;
mod render;
mod server;
mod streamdeck;
mod text;
mod tls;
mod ui;
mod vt;
mod webrtc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use config::Config;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{error, info, warn};

#[derive(Parser)]
#[command(
    name = "streame",
    version,
    about = "Régie vidéo : téléphone (WebRTC) → HDMI, scènes, multiview, Stream Deck"
)]
struct Cli {
    /// Fichier de configuration TOML.
    #[arg(short, long, default_value = "streame.toml")]
    config: PathBuf,

    /// Pas de fenêtres (serveur, audio et API seulement) — tests sans écran.
    #[arg(long)]
    no_window: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Écrit un fichier de configuration d'exemple (et une image d'habillage de démo).
    Init,
    /// Liste les périphériques audio et les écrans détectés.
    Devices,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli = Cli::parse();
    let _ = rustls::crypto::ring::default_provider().install_default();

    match cli.command {
        Some(Command::Init) => return cmd_init(&cli.config),
        Some(Command::Devices) => return cmd_devices(),
        None => {}
    }

    let cfg = Arc::new(if cli.config.is_file() {
        Config::load(&cli.config)?
    } else {
        warn!(
            "{} introuvable : configuration par défaut (voir `streame init`)",
            cli.config.display()
        );
        let mut c = Config::default();
        c.validate()?;
        c
    });

    // Certificat TLS (inclut les IP locales dans le certificat).
    let ips: Vec<String> = local_ips();
    let tls_files = tls::load_or_create(&cfg.resolve(&cfg.server.cert_dir), &ips)?;

    // Moteur.
    let engine = engine::Engine::new(cfg.clone())?;

    // Serveur HTTPS + signaling dans un runtime tokio sur un thread dédié.
    // Runtime Tokio dédié au média (WebRTC, boucles RTP, Opus, décodage) : isolé du serveur
    // HTTP/WS pour qu'un blocage ou une charge média ne rende jamais la page ni le signaling
    // inaccessibles — et inversement.
    let media_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_name("media")
        .enable_all()
        .build()
        .context("runtime média")?;
    server::spawn_lag_watchdog(media_rt.handle(), "média");
    let state = Arc::new(server::AppState {
        cfg: cfg.clone(),
        engine: engine.clone(),
        phone: std::sync::Mutex::new(None),
        media: media_rt.handle().clone(),
    });
    let server_thread = {
        let state = state.clone();
        std::thread::Builder::new()
            .name("server".into())
            .spawn(move || {
                let rt = tokio::runtime::Runtime::new().expect("runtime tokio");
                if let Err(e) =
                    rt.block_on(server::run(state, tls_files.cert_pem, tls_files.key_pem))
                {
                    error!("serveur : {e:#}");
                }
            })?
    };

    let _deck = streamdeck::spawn(cfg.clone(), engine.clone());
    engine.start()?;
    print_urls(&cfg, &ips);

    // Ctrl-C : arrêt propre (ferme les flux CoreAudio, sinon la carte USB peut rester coincée).
    {
        let engine = engine.clone();
        let _ = ctrlc::set_handler(move || {
            info!("arrêt (Ctrl-C)");
            engine.stop();
            std::process::exit(0);
        });
    }

    if cli.no_window {
        info!("mode sans fenêtre : Ctrl-C pour quitter");
        let _ = server_thread.join();
    } else {
        ui::run(cfg.clone(), engine.clone())?;
        info!("fermeture");
    }
    if let Some(p) = state.phone.lock().unwrap().take() {
        p.close();
    }
    engine.stop();
    std::process::exit(0);
}

fn local_ips() -> Vec<String> {
    let mut ips = Vec::new();
    if let Ok(list) = local_ip_address::list_afinet_netifas() {
        for (_, ip) in list {
            if let std::net::IpAddr::V4(v4) = ip {
                if !v4.is_loopback() && !v4.is_link_local() {
                    ips.push(v4.to_string());
                }
            }
        }
    }
    if let Ok(ip) = local_ip_address::local_ip() {
        let s = ip.to_string();
        if !ips.contains(&s) {
            ips.insert(0, s);
        }
    }
    ips
}

fn print_urls(cfg: &Config, ips: &[String]) {
    let port = cfg.server.bind.rsplit(':').next().unwrap_or("8443");
    let host = ips.first().cloned().unwrap_or_else(|| "localhost".into());
    let url = format!("https://{host}:{port}/");
    println!();
    println!("  Téléphone  : {url}");
    for ip in ips.iter().skip(1) {
        println!("               https://{ip}:{port}/");
    }
    println!("  Contrôle   : https://{host}:{port}/control");
    println!("  (certificat auto-signé : accepter l'avertissement du navigateur)");
    if let Ok(code) = qrcode::QrCode::new(url.as_bytes()) {
        let s = code
            .render::<qrcode::render::unicode::Dense1x2>()
            .quiet_zone(true)
            .build();
        println!();
        for line in s.lines() {
            println!("  {line}");
        }
    }
    println!();
}

fn cmd_init(path: &std::path::Path) -> Result<()> {
    anyhow::ensure!(!path.exists(), "{} existe déjà", path.display());
    let cfg = Config::example();
    let text = toml::to_string_pretty(&cfg)?;
    std::fs::write(path, format!("{}\n{}", EXAMPLE_HEADER, text))?;
    println!("configuration écrite : {}", path.display());
    let dir = path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default()
        .join("assets");
    std::fs::create_dir_all(&dir)?;
    let lower = dir.join("lower-third.png");
    if !lower.exists() {
        write_demo_lower_third(&lower, cfg.video.width as u32, cfg.video.height as u32)?;
        println!("image de démo écrite : {}", lower.display());
    }
    println!(
        "(placez une vidéo dans {} pour la scène « overlay »)",
        dir.join("overlay.mp4").display()
    );
    Ok(())
}

const EXAMPLE_HEADER: &str =
    "# Configuration Streame — voir README.md pour le détail des options.\n";

/// Bandeau semi-transparent en bas de l'image (démo d'habillage).
fn write_demo_lower_third(path: &std::path::Path, w: u32, h: u32) -> Result<()> {
    let mut img = image::RgbaImage::from_pixel(w, h, image::Rgba([0, 0, 0, 0]));
    let band_top = h * 82 / 100;
    let band_bottom = h * 92 / 100;
    for y in band_top..band_bottom {
        for x in (w / 16)..(w * 9 / 16) {
            let edge = (x - w / 16) as f32 / (w / 2) as f32;
            let alpha = (220.0 * (1.0 - edge * 0.6)) as u8;
            img.put_pixel(x, y, image::Rgba([10, 60, 160, alpha]));
        }
    }
    for y in (band_top - 8)..band_top {
        for x in (w / 16)..(w * 9 / 16) {
            img.put_pixel(x, y, image::Rgba([255, 200, 0, 255]));
        }
    }
    img.save(path)?;
    Ok(())
}

fn cmd_devices() -> Result<()> {
    println!("Périphériques audio :");
    for d in audio::list_devices() {
        println!(
            "  [{}] {}{}",
            d.class,
            d.name,
            d.max_channels
                .map(|c| format!(" ({c} canaux max)"))
                .unwrap_or_default()
        );
    }
    println!();
    println!("Écrans :");
    match ui::list_monitors() {
        Ok(monitors) => {
            for (i, m) in monitors.iter().enumerate() {
                println!(
                    "  [{i}] {} — {}x{} @ ({}, {})",
                    m.name, m.width, m.height, m.x, m.y
                );
            }
        }
        Err(e) => println!("  (indisponibles : {e})"),
    }
    Ok(())
}

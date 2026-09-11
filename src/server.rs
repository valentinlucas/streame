//! Serveur HTTPS : page du téléphone, signaling WebSocket, page/API de contrôle.

use crate::config::Config;
use crate::engine::{Engine, Event};
use crate::webrtc::{ClientMsg, PhoneSession, ServerMsg};
use anyhow::{Context, Result};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::{info, warn};

pub struct AppState {
    pub cfg: Arc<Config>,
    pub engine: Arc<Engine>,
    pub phone: Mutex<Option<Arc<PhoneSession>>>,
}

pub type Shared = Arc<AppState>;

pub async fn run(state: Shared, cert_pem: Vec<u8>, key_pem: Vec<u8>) -> Result<()> {
    let addr: SocketAddr = state
        .cfg
        .server
        .bind
        .parse()
        .context("server.bind invalide")?;
    let app = Router::new()
        .route(
            "/",
            get(|| async { Html(include_str!("../web/index.html")) }),
        )
        .route(
            "/app.js",
            get(|| async { js(include_str!("../web/app.js")) }),
        )
        .route(
            "/control",
            get(|| async { Html(include_str!("../web/control.html")) }),
        )
        .route(
            "/control.js",
            get(|| async { js(include_str!("../web/control.js")) }),
        )
        .route(
            "/style.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("../web/style.css"),
                )
            }),
        )
        .route(
            "/manifest.webmanifest",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "application/manifest+json")],
                    include_str!("../web/manifest.webmanifest"),
                )
            }),
        )
        .route("/ws", get(phone_ws))
        .route("/ws/control", get(control_ws))
        .route("/api/state", get(api_state))
        .route("/api/program/{id}", post(api_program))
        .route("/api/cut/{id}", post(api_cut))
        .route("/api/preview/{id}", post(api_preview))
        .route("/api/take", post(api_take))
        .route("/api/audio", get(api_audio))
        .with_state(state);

    let tls = axum_server::tls_rustls::RustlsConfig::from_pem(cert_pem, key_pem)
        .await
        .context("configuration TLS")?;
    info!("serveur HTTPS sur https://{addr}");
    axum_server::bind_rustls(addr, tls)
        .serve(app.into_make_service())
        .await
        .context("serveur HTTPS")?;
    Ok(())
}

fn js(body: &'static str) -> Response {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

// ---------------------------------------------------------------------------------------------
// Signaling téléphone
// ---------------------------------------------------------------------------------------------

async fn phone_ws(ws: WebSocketUpgrade, State(state): State<Shared>) -> Response {
    ws.on_upgrade(move |socket| handle_phone(socket, state))
}

async fn handle_phone(socket: WebSocket, state: Shared) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMsg>();
    let mut session: Option<Arc<PhoneSession>> = None;
    let mut events = state.engine.subscribe();

    loop {
        tokio::select! {
            msg = stream.next() => {
                let text = match msg {
                    Some(Ok(Message::Text(t))) => t.to_string(),
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => { warn!("websocket téléphone : {e}"); break; }
                };
                let parsed: ClientMsg = match serde_json::from_str(&text) {
                    Ok(m) => m,
                    Err(e) => { warn!("message téléphone invalide : {e}"); continue; }
                };
                match parsed {
                    ClientMsg::Hello { name } => {
                        let name = name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "Téléphone".into());
                        // Un seul téléphone à la fois : le nouveau remplace l'ancien.
                        if let Some(old) = state.phone.lock().unwrap().take() {
                            info!("téléphone « {} » remplacé par « {name} »", old.name);
                            old.close();
                        }
                        match PhoneSession::start(&state.cfg, name, tx.clone(), state.engine.clone()) {
                            Ok(s) => {
                                *state.phone.lock().unwrap() = Some(s.clone());
                                session = Some(s);
                                let _ = tx.send(ServerMsg::OnAir { on: state.engine.phone_on_air() });
                            }
                            Err(e) => {
                                warn!("session WebRTC : {e:#}");
                                let _ = tx.send(ServerMsg::Error { message: format!("{e:#}") });
                            }
                        }
                    }
                    ClientMsg::Answer { sdp } => {
                        if let Some(s) = &session {
                            if let Err(e) = s.set_answer(&sdp) {
                                warn!("réponse SDP : {e:#}");
                            }
                        }
                    }
                    ClientMsg::Ice { candidate, sdp_m_line_index } => {
                        if let Some(s) = &session {
                            s.add_ice(sdp_m_line_index, &candidate);
                        }
                    }
                    ClientMsg::Stats(st) => {
                        if let Some(s) = &session {
                            s.set_phone_stats(&state.engine, st);
                        }
                    }
                    ClientMsg::Bye => break,
                }
            }
            out = rx.recv() => {
                match out {
                    Some(m) => {
                        let json = serde_json::to_string(&m).unwrap_or_default();
                        if sink.send(Message::Text(json.into())).await.is_err() { break; }
                    }
                    None => break,
                }
            }
            ev = events.recv() => {
                // Le programme a changé (ou le téléphone s'est (dé)connecté) : on informe la
                // page de son état « à l'antenne ». Un second envoi après la durée du fondu
                // capte la fin d'une transition (ex. la scène du téléphone qui s'efface).
                if matches!(ev, Ok(Event::Program { .. }) | Ok(Event::Phone { .. })) {
                    let _ = tx.send(ServerMsg::OnAir { on: state.engine.phone_on_air() });
                    let dur = state.cfg.transition.duration_ms;
                    if dur > 0 {
                        let tx2 = tx.clone();
                        let engine = state.engine.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(dur + 80)).await;
                            let _ = tx2.send(ServerMsg::OnAir { on: engine.phone_on_air() });
                        });
                    }
                }
            }
            _ = async {
                match &session {
                    Some(s) => s.cancel.cancelled().await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                let _ = sink.send(Message::Text(serde_json::to_string(&ServerMsg::Bye { reason: "session terminée".into() }).unwrap().into())).await;
                break;
            }
        }
    }

    if let Some(s) = session {
        let mut cur = state.phone.lock().unwrap();
        if cur.as_ref().map(|c| Arc::ptr_eq(c, &s)).unwrap_or(false) {
            cur.take();
            state.engine.set_phone(None);
        }
        drop(cur);
        s.close();
        info!("téléphone « {} » déconnecté", s.name);
    }
}

// ---------------------------------------------------------------------------------------------
// Contrôle (page web / API)
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ControlMsg {
    Program {
        scene: String,
    },
    Cut {
        scene: String,
    },
    Preview {
        scene: String,
    },
    Take,
    /// Re-route une source audio en direct : target = "stream"|"branding"|"return".
    AudioRoute {
        target: String,
        channels: Vec<usize>,
    },
}

async fn control_ws(ws: WebSocketUpgrade, State(state): State<Shared>) -> Response {
    ws.on_upgrade(move |socket| handle_control(socket, state))
}

async fn handle_control(socket: WebSocket, state: Shared) {
    let (mut sink, mut stream) = socket.split();
    let mut events = state.engine.subscribe();
    let send_state = |snap: crate::engine::Snapshot| {
        serde_json::json!({ "type": "state", "state": snap }).to_string()
    };
    if sink
        .send(Message::Text(send_state(state.engine.snapshot()).into()))
        .await
        .is_err()
    {
        return;
    }
    loop {
        tokio::select! {
            msg = stream.next() => {
                let text = match msg {
                    Some(Ok(Message::Text(t))) => t.to_string(),
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(_)) => continue,
                };
                if let Ok(cmd) = serde_json::from_str::<ControlMsg>(&text) {
                    apply_control(&state.engine, cmd);
                }
            }
            ev = events.recv() => {
                match ev {
                    Ok(Event::Program { .. }) | Ok(Event::Preview { .. }) | Ok(Event::Phone { .. }) => {
                        if sink.send(Message::Text(send_state(state.engine.snapshot()).into())).await.is_err() { break; }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        }
    }
}

fn apply_control(engine: &Arc<Engine>, cmd: ControlMsg) -> bool {
    match cmd {
        ControlMsg::Program { scene } => engine
            .scene_index(&scene)
            .map(|i| engine.transition_to(i))
            .is_some(),
        ControlMsg::Cut { scene } => engine.scene_index(&scene).map(|i| engine.cut(i)).is_some(),
        ControlMsg::Preview { scene } => engine
            .scene_index(&scene)
            .map(|i| engine.set_preview(i))
            .is_some(),
        ControlMsg::Take => {
            engine.take();
            true
        }
        ControlMsg::AudioRoute { target, channels } => engine.set_audio_route(&target, &channels),
    }
}

async fn api_state(State(state): State<Shared>) -> Json<crate::engine::Snapshot> {
    Json(state.engine.snapshot())
}

/// Périphériques audio détectés et routage courant (pour l'écran de sélection audio).
async fn api_audio(State(state): State<Shared>) -> Json<serde_json::Value> {
    let devices: Vec<serde_json::Value> = crate::audio::list_devices()
        .into_iter()
        .map(|d| {
            let dir = if d.class.contains("Sink") {
                "sortie"
            } else {
                "entrée"
            };
            serde_json::json!({ "name": d.name, "direction": dir, "channels": d.max_channels })
        })
        .collect();
    Json(serde_json::json!({
        "devices": devices,
        "routing": state.engine.audio_routing(),
        "output_device": state.cfg.audio.output_device,
        "input_device": state.cfg.audio.input_device,
        "sample_rate": state.cfg.audio.sample_rate,
    }))
}

fn api_result(ok: bool, state: &Shared) -> Response {
    if ok {
        Json(state.engine.snapshot()).into_response()
    } else {
        (StatusCode::NOT_FOUND, "scène inconnue").into_response()
    }
}

async fn api_program(State(state): State<Shared>, Path(id): Path<String>) -> Response {
    api_result(
        apply_control(&state.engine, ControlMsg::Program { scene: id }),
        &state,
    )
}

async fn api_cut(State(state): State<Shared>, Path(id): Path<String>) -> Response {
    api_result(
        apply_control(&state.engine, ControlMsg::Cut { scene: id }),
        &state,
    )
}

async fn api_preview(State(state): State<Shared>, Path(id): Path<String>) -> Response {
    api_result(
        apply_control(&state.engine, ControlMsg::Preview { scene: id }),
        &state,
    )
}

async fn api_take(State(state): State<Shared>) -> Response {
    api_result(apply_control(&state.engine, ControlMsg::Take), &state)
}
